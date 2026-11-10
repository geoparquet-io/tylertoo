//! A throwaway localhost HTTP/1.1 object server for remote-input tests.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::Arc;

/// Serve one byte blob with Range support, as a no-network object store.
///
/// Answers `HEAD` (size) and ranged/full `GET`
/// exactly as `object_store`'s HTTP store expects, at any path. With
/// `required_query` set, a request whose query string is not exactly that
/// gets `403 Forbidden`, and so does any `HEAD`: an S3 or GCS presigned URL
/// signs one method, GET, and its host refuses the rest (#820).
///
/// Returns the base URL (`http://127.0.0.1:PORT`); the accept loop runs on a
/// detached thread until the test process exits. Binding happens before
/// return, so the listen backlog absorbs any connect that races the accept
/// loop.
///
/// # Panics
///
/// Panics if no localhost port can be bound.
pub fn serve_bytes_over_http(body: Vec<u8>, required_query: Option<&str>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let body = Arc::new(body);
    let required: Option<Arc<str>> = required_query.map(Arc::from);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let body = Arc::clone(&body);
            let required = required.clone();
            std::thread::spawn(move || serve_connection(stream, &body, required.as_deref()));
        }
    });
    format!("http://{addr}")
}

/// One connection, which may carry several keep-alive requests.
fn serve_connection(mut stream: std::net::TcpStream, body: &[u8], required: Option<&str>) {
    let Ok(peer) = stream.try_clone() else { return };
    let mut reader = BufReader::new(peer);
    loop {
        let mut request_line = String::new();
        match reader.read_line(&mut request_line) {
            Ok(0) | Err(_) => break, // client closed
            Ok(_) => {}
        }
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        if method.is_empty() {
            break;
        }
        let query = parts.next().and_then(|t| t.split_once('?')).map(|(_, q)| q);
        let range = read_range_header(&mut reader, body.len() as u64);
        let response = if required.is_some_and(|r| query != Some(r) || method == "HEAD") {
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_vec()
        } else {
            respond(&method, range, body)
        };
        if stream.write_all(&response).is_err() {
            break;
        }
        let _ = stream.flush();
    }
}

/// Consume the request headers, returning the (clamped, inclusive) `Range`.
fn read_range_header(reader: &mut impl BufRead, size: u64) -> Option<(u64, u64)> {
    let mut range = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" || header == "\n" {
            return range;
        }
        let lower = header.to_ascii_lowercase();
        let Some(spec) = lower
            .strip_prefix("range:")
            .and_then(|v| v.trim().strip_prefix("bytes="))
        else {
            continue;
        };
        let spec = spec.split(',').next().unwrap_or("").trim();
        let (a, b) = spec.split_once('-').unwrap_or((spec, ""));
        range = Some(if a.is_empty() {
            // Suffix range: the last N bytes.
            let n: u64 = b.trim().parse().unwrap_or(0);
            (size.saturating_sub(n), size.saturating_sub(1))
        } else {
            let start = a.trim().parse().unwrap_or(0);
            let end = b.trim().parse().unwrap_or(size.saturating_sub(1));
            (start, end.min(size.saturating_sub(1)))
        });
    }
}

fn respond(method: &str, range: Option<(u64, u64)>, body: &[u8]) -> Vec<u8> {
    let size = body.len();
    let (status, extra, slice) = match (method, range) {
        ("HEAD", _) => ("200 OK", String::new(), &body[..0]),
        ("GET", Some((start, end))) => (
            "206 Partial Content",
            format!("Content-Range: bytes {start}-{end}/{size}\r\n"),
            &body[start as usize..=end as usize],
        ),
        ("GET", None) => ("200 OK", String::new(), body),
        _ => return b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n".to_vec(),
    };
    // HEAD answers with the full object's length and no body.
    let len = if method == "HEAD" { size } else { slice.len() };
    let mut resp = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\n\
         Accept-Ranges: bytes\r\n{extra}Content-Length: {len}\r\n\r\n"
    )
    .into_bytes();
    resp.extend_from_slice(slice);
    resp
}
