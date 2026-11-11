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
use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tiny_http::{Header, Method, Request, Response, StatusCode};

/// Responses in flight at once.
pub const MAX_RESPONSES: usize = 64;

const VIEWER: &str = include_str!("viewer.html");

/// Why `serve` could not start or keep listening.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// An archive path has no UTF-8 file stem to name it by.
    #[error("{}: an archive needs a UTF-8 file name", path.display())]
    #[non_exhaustive]
    BadName { path: PathBuf },
    /// No archive was given.
    #[error("no archive to serve")]
    NoArchives,
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
            | ServeError::NoArchives
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
    /// The file changed and has not reopened since.
    broken: bool,
    /// `/stats.json` for this version, computed on its first request.
    stats: Option<Vec<u8>>,
}

impl Served {
    /// Reopen the archive if the file changed, and say whether it can be
    /// served. A changed file that does not open (caught mid-rewrite,
    /// removed, or damaged) is refused until it opens: a file rewritten in
    /// place would otherwise be read at the old directory's offsets.
    fn refresh(&mut self) -> bool {
        let Ok(now) = std::fs::metadata(&self.path).map(|m| stamp(&m)) else {
            self.broken = true;
            return false;
        };
        if now == self.stamp && !self.broken {
            return true;
        }
        match ArchiveIndex::open(&self.path) {
            Ok(index) => {
                log::info!("reloaded {}", self.path.display());
                warn_variable_depth(&index);
                self.index = index;
                self.stamp = now;
                self.broken = false;
                self.stats = None;
                true
            }
            Err(e) => {
                log::warn!("{} changed and does not open: {e}", self.path.display());
                self.broken = true;
                false
            }
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
    http: Arc<tiny_http::Server>,
    archives: Vec<Served>,
    cors: Vec<String>,
    /// Archive responses being sent, at most [`MAX_RESPONSES`].
    busy: usize,
    /// Connections with a response being sent, and the requests pipelined
    /// behind it, which cannot be answered before it ends.
    sending: HashMap<SocketAddr, VecDeque<Request>>,
    /// Each response thread reports here when it ends: its connection, and
    /// whether it counted in `busy`.
    done: (Sender<Done>, Receiver<Done>),
}

type Done = (Option<SocketAddr>, bool);

impl Server {
    /// Open every archive and bind `host:port` (port 0 picks a free port).
    /// `cors` lists the origins whose pages may read the responses (`*` for
    /// any); empty, no other origin may.
    ///
    /// # Errors
    ///
    /// An archive that does not open, two archives with the same file stem,
    /// or an address that cannot be bound.
    pub fn bind(
        archives: &[PathBuf],
        host: &str,
        port: u16,
        cors: &[String],
    ) -> Result<Self, ServeError> {
        if archives.is_empty() {
            return Err(ServeError::NoArchives);
        }
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
                broken: false,
                stats: None,
            });
        }
        let http = tiny_http::Server::http((host, port)).map_err(|error| ServeError::Bind {
            addr: format!("{host}:{port}"),
            error,
        })?;
        Ok(Server {
            http: Arc::new(http),
            archives: served,
            cors: cors.to_vec(),
            busy: 0,
            sending: HashMap::new(),
            done: mpsc::channel(),
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
        let mut result = Ok(());
        while keep_going() {
            let ended: Vec<Done> = self.done.1.try_iter().collect();
            for (addr, counted) in ended {
                self.busy -= usize::from(counted);
                let Some(addr) = addr else { continue };
                // The connection is free: its next pipelined request goes.
                let next = self.sending.get_mut(&addr).and_then(VecDeque::pop_front);
                match next {
                    Some(req) => self.send(req, Some(addr)),
                    None => {
                        self.sending.remove(&addr);
                    }
                }
            }
            let req = match self.http.recv_timeout(Duration::from_millis(200)) {
                Ok(req) => req,
                Err(e) => {
                    result = Err(e.into());
                    break;
                }
            };
            if let Some(req) = req {
                let addr = req.remote_addr().copied();
                match addr.and_then(|a| self.sending.get_mut(&a)) {
                    Some(queue) => queue.push_back(req),
                    None => self.send(req, addr),
                }
            }
        }
        // A queued request answers 500 when dropped, after the response
        // ahead of it: drop them off this thread, or a stalled download
        // would hold up the stop.
        let queued = std::mem::take(&mut self.sending);
        std::thread::spawn(move || drop(queued));
        result
    }

    /// Answer `req` on a thread of its own, so that a client which stops
    /// reading stalls neither other requests nor the `keep_going` poll.
    /// Past [`MAX_RESPONSES`] archive responses the answer is an empty 503.
    /// One thread per connection at most, as requests pipelined behind a
    /// response wait in `sending` for it to end.
    fn send(&mut self, req: Request, addr: Option<SocketAddr>) {
        let counted = self.busy < MAX_RESPONSES;
        let mut res = if counted {
            let host = request_header(&req, "Host").map(str::to_owned);
            self.respond(&req, host.as_deref())
        } else {
            status(503).with_header(header("Retry-After", "1"))
        };
        for h in self.cors_headers(&req) {
            res.add_header(h);
        }
        self.busy += usize::from(counted);
        if let Some(a) = addr {
            self.sending.entry(a).or_default();
        }
        let done = self.done.0.clone();
        // Weak: a stalled download must not keep the listener open after
        // `run` returns.
        let wake = Arc::downgrade(&self.http);
        std::thread::spawn(move || {
            // A client that hung up mid-response is not the server's error.
            let _ = req.respond(res);
            let _ = done.send((addr, counted));
            // A keep-alive client's next request may already be waiting on
            // this one; wake the loop rather than leave it to the poll.
            if let Some(http) = wake.upgrade() {
                http.unblock();
            }
        });
    }

    /// The CORS headers for `req`: none unless `--cors` allows its origin.
    fn cors_headers(&self, req: &Request) -> Vec<Header> {
        let any = self.cors.iter().any(|c| c == "*");
        let allow = if any {
            Some("*")
        } else {
            request_header(req, "Origin").filter(|o| self.cors.iter().any(|c| c == o))
        };
        let mut out = Vec::new();
        if !self.cors.is_empty() && !any {
            out.push(header("Vary", "Origin"));
        }
        if let Some(origin) = allow {
            out.extend([
                header("Access-Control-Allow-Origin", origin),
                header("Access-Control-Allow-Methods", "GET, HEAD, OPTIONS"),
                // A Range or conditional request from another origin is
                // preflighted for these.
                header("Access-Control-Allow-Headers", "Range, If-Match, If-Range"),
                header("Access-Control-Expose-Headers", "ETag, Content-Range"),
            ]);
        }
        out
    }

    fn respond(&mut self, req: &Request, host: Option<&str>) -> Response<Box<dyn Read + Send>> {
        match req.method() {
            Method::Get | Method::Head => {}
            Method::Options => {
                return status(204).with_header(header("Allow", "GET, HEAD, OPTIONS"));
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
                let a = match self.archive(name) {
                    Ok(a) => a,
                    Err(res) => return res,
                };
                match ext {
                    "json" => json(&tilejson(a, host.unwrap_or("localhost"))),
                    "pmtiles" => raw(&a.path, req),
                    _ => status(404),
                }
            }
            [name, "metadata.json"] => match self.archive(name) {
                Ok(a) => metadata(&a.index),
                Err(res) => res,
            },
            [name, "stats.json"] => match self.archive(name) {
                Ok(a) => {
                    if a.stats.is_none() {
                        match compute_stats(&a.index, DEFAULT_LARGEST, false) {
                            Ok(report) => {
                                a.stats = Some(serde_json::json!(report).to_string().into_bytes());
                            }
                            Err(e) => return error(&e),
                        }
                    }
                    body(a.stats.clone().unwrap_or_default(), "application/json")
                }
                Err(res) => res,
            },
            [name, z, x, y] => {
                let y = y.split_once('.').map_or(*y, |(y, _)| y);
                let (Ok(z), Ok(x), Ok(y)) = (z.parse(), x.parse(), y.parse()) else {
                    return status(404);
                };
                match self.archive(name) {
                    Ok(a) => {
                        let res = tile(&a.index, z, x, y);
                        // A rewrite in place that began after `refresh` has
                        // moved the bytes under the old offsets.
                        let same = std::fs::metadata(&a.path).is_ok_and(|m| stamp(&m) == a.stamp);
                        if same {
                            res
                        } else {
                            status(503)
                        }
                    }
                    Err(res) => res,
                }
            }
            _ => status(404),
        }
    }

    /// The archive named `name`, or the response that replaces it: 404 for
    /// no such archive, 503 while a changed file does not reopen.
    fn archive(&mut self, name: &str) -> Result<&mut Served, Response<Box<dyn Read + Send>>> {
        let a = self
            .archives
            .iter_mut()
            .find(|a| a.name == name)
            .ok_or_else(|| status(404))?;
        if a.refresh() {
            Ok(a)
        } else {
            Err(status(503))
        }
    }
}

fn request_header<'r>(req: &'r Request, name: &'static str) -> Option<&'r str> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().trim())
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
    let (file, version) = match File::open(path).and_then(|f| {
        let m = f.metadata()?;
        Ok((f, stamp(&m)))
    }) {
        Ok(v) => v,
        Err(e) => return error(&e),
    };
    let len = version.0;
    let tag = etag(version);
    let get = |name| request_header(req, name);
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
    let n = end - start;
    let mut reader = Unchanged {
        file: file.take(n),
        version,
    };
    if let Err(e) = reader.file.get_mut().seek(SeekFrom::Start(start)) {
        return error(&e);
    }
    let mut headers = vec![
        header("Content-Type", "application/vnd.pmtiles"),
        header("Accept-Ranges", "bytes"),
        header("ETag", &tag),
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
    // A range (what a PMTiles client reads) is read whole before its status
    // goes out, so a rewrite caught by the read is answered, not sent: 412
    // to a conditional request, whose client refetches the header, else 503.
    // A larger body streams, and a rewrite cuts it off short of its length.
    let body: Box<dyn Read + Send> = if code == 206 && n <= MAX_TILE_BYTES {
        let mut bytes = Vec::with_capacity(n_usize);
        if let Err(e) = reader.read_to_end(&mut bytes) {
            if !reader.unchanged() {
                let conditional = get("If-Match").is_some() || get("If-Range").is_some();
                return status(if conditional { 412 } else { 503 });
            }
            return error(&e);
        }
        Box::new(std::io::Cursor::new(bytes))
    } else {
        Box::new(reader)
    };
    Response::new(StatusCode(code), headers, body, Some(n_usize), None)
}

/// Bytes of the archive while it stays the version `version` names. Each
/// read checks the file after it, so no byte of a later version is
/// returned: a rename over the path leaves this handle on the old file,
/// and an in-place rewrite fails the read.
// shortcut: tiny_http cannot close a keep-alive connection from the
// response side, so after a failed read an HTTP/1.1 client waits for the
// missing bytes until it gives up; move to an HTTP layer that can abort a
// connection if whole-file downloads of rewritten archives matter.
struct Unchanged {
    file: std::io::Take<File>,
    version: Stamp,
}

impl Unchanged {
    fn unchanged(&self) -> bool {
        self.file
            .get_ref()
            .metadata()
            .is_ok_and(|m| stamp(&m) == self.version)
    }
}

impl Read for Unchanged {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.file.read(buf)?;
        if self.unchanged() {
            Ok(n)
        } else {
            Err(std::io::Error::other("the archive changed during the read"))
        }
    }
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
