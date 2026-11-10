//! `tylertoo serve` (#825): tiles, ranges and metadata come back as the
//! archive's own bytes, and a rewritten archive is picked up.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use tylertoo_core::internal::archive_index::ArchiveIndex;
use tylertoo_core::internal::pmtiles_writer::StreamingPmtilesWriter;
use tylertoo_core::internal::serve::Server;
use tylertoo_core::Compression;

fn write_archive(path: &Path, tag: u8) {
    let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
    w.set_layer_name("l");
    for (i, (z, x, y)) in [(0u8, 0u32, 0u32), (1, 0, 0), (1, 1, 1), (2, 3, 1)]
        .into_iter()
        .enumerate()
    {
        w.add_tile(z, x, y, &[tag, i as u8, 1, 2, 3]).unwrap();
    }
    w.finalize(path).unwrap();
}

/// `GET path` with extra header lines: (status, headers, body).
fn get(addr: &str, path: &str, extra: &str) -> (u16, String, Vec<u8>) {
    request(addr, "GET", path, extra)
}

fn request(addr: &str, method: &str, path: &str, extra: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "{method} {path} HTTP/1.0\r\nHost: {addr}\r\n{extra}\r\n").unwrap();
    let mut r = BufReader::new(s);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        head.push_str(&line);
    }
    let mut body = Vec::new();
    r.read_to_end(&mut body).unwrap();
    let code = head[9..12].parse().unwrap();
    (code, head.to_ascii_lowercase(), body)
}

#[test]
fn serves_the_archives_own_bytes_and_reloads_a_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    // A space in the name: URL segments are percent-decoded.
    let path = dir.path().join("my demo.pmtiles");
    write_archive(&path, 1);

    let server = Server::bind(std::slice::from_ref(&path), "127.0.0.1", 0).unwrap();
    let url = server.url();
    let addr = url
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned();
    std::thread::spawn(move || server.run(|| true));

    let index = ArchiveIndex::open(&path).unwrap();
    for t in index.tiles() {
        let t = t.unwrap();
        let (code, head, body) = get(
            &addr,
            &format!("/my%20demo/{}/{}/{}.mvt", t.z, t.x, t.y),
            "",
        );
        assert_eq!(code, 200);
        assert!(head.contains("content-encoding: gzip"), "{head}");
        assert_eq!(body, index.read_range(t.range).unwrap());
    }
    assert_eq!(get(&addr, "/my%20demo/2/0/0.mvt", "").0, 204);
    assert_eq!(get(&addr, "/nope/0/0/0.mvt", "").0, 404);
    // The CORS preflight of a cross-origin Range request.
    let (code, head, _) = request(&addr, "OPTIONS", "/my%20demo.pmtiles", "");
    assert_eq!(code, 204);
    assert!(
        head.contains("access-control-allow-headers: range, if-match, if-range"),
        "{head}"
    );

    let file = std::fs::read(&path).unwrap();
    let (code, head, body) = get(&addr, "/my%20demo.pmtiles", "Range: bytes=0-126\r\n");
    assert_eq!(code, 206);
    assert!(
        head.contains(&format!("content-range: bytes 0-126/{}", file.len())),
        "{head}"
    );
    assert_eq!(body, file[..127]);
    let (code, _, body) = get(&addr, "/my%20demo.pmtiles", "Range: bytes=-10\r\n");
    assert_eq!((code, body.as_slice()), (206, &file[file.len() - 10..]));
    let (_, head, body) = get(&addr, "/my%20demo.pmtiles", "");
    assert_eq!(body, file);
    let etag = |head: &str| {
        head.lines()
            .find(|l| l.starts_with("etag:"))
            .map(str::to_owned)
    };
    let before = etag(&head).expect("etag");

    let meta = ArchiveIndex::read_metadata_only(&path).unwrap();
    assert_eq!(get(&addr, "/my%20demo/metadata.json", "").2, meta);
    let (_, _, tj) = get(&addr, "/my%20demo.json", "");
    let tj: serde_json::Value = serde_json::from_slice(&tj).unwrap();
    assert_eq!(
        tj["tiles"][0],
        format!("http://{addr}/my%20demo/{{z}}/{{x}}/{{y}}.mvt")
    );
    assert_eq!(tj["encoding"], "mvt");
    assert!(String::from_utf8(get(&addr, "/", "").2)
        .unwrap()
        .contains("maplibre-gl"));

    // A rebuild replaces the file; the next request sees the new tiles.
    let next = dir.path().join("next.pmtiles");
    write_archive(&next, 2);
    std::fs::rename(&next, &path).unwrap();
    let index = ArchiveIndex::open(&path).unwrap();
    let r = index.find(0, 0, 0).unwrap().unwrap();
    assert_eq!(
        get(&addr, "/my%20demo/0/0/0.mvt", "").2,
        index.read_range(r).unwrap()
    );
    let (_, head, _) = get(&addr, "/my%20demo.pmtiles", "Range: bytes=0-9\r\n");
    assert_ne!(
        etag(&head),
        Some(before.clone()),
        "a rebuild changes the ETag"
    );
    // A client still holding the old version's offsets is told so.
    let old_tag = before.trim_start_matches("etag:").trim();
    let cond = format!("Range: bytes=0-9\r\nIf-Match: {old_tag}\r\n");
    assert_eq!(get(&addr, "/my%20demo.pmtiles", &cond).0, 412);
    let cond = format!("Range: bytes=0-9\r\nIf-Range: {old_tag}\r\n");
    assert_eq!(get(&addr, "/my%20demo.pmtiles", &cond).0, 200);
    let now = etag(&head).unwrap();
    let cond = format!(
        "Range: bytes=0-9\r\nIf-Match: {}\r\n",
        now.trim_start_matches("etag:").trim()
    );
    assert_eq!(get(&addr, "/my%20demo.pmtiles", &cond).0, 206);
}

#[test]
fn index_is_a_reserved_archive_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.pmtiles");
    write_archive(&path, 1);
    let err = Server::bind(&[path], "127.0.0.1", 0).err().unwrap();
    assert!(err.to_string().contains("reserved"), "{err}");
}
