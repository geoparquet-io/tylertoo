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
    // A server that stops answering fails the test instead of hanging it.
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
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
    // Written without bounds: the world, not the writer's inverted box.
    assert_eq!(tj["bounds"][0], -180.0);
    assert!(tj["bounds"][0].as_f64() < tj["bounds"][2].as_f64(), "{tj}");
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
    let err = Server::bind(&[], "127.0.0.1", 0).err().unwrap();
    assert!(err.to_string().contains("no archive"), "{err}");
}

/// A client stalled mid-download of a whole archive blocks neither other
/// requests nor the server's stop poll.
#[test]
fn a_stalled_download_blocks_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.pmtiles");
    let mut w = StreamingPmtilesWriter::new(Compression::None).unwrap();
    w.set_layer_name("l");
    // ~32 MiB of distinct bodies: far more than the socket buffers hold.
    for i in 0..512u32 {
        let body: Vec<u8> = (0..65536u32)
            .map(|j| (i ^ j.wrapping_mul(2654435761)) as u8)
            .collect();
        w.add_tile(9, i, 0, &body).unwrap();
    }
    w.finalize(&path).unwrap();

    let server = Server::bind(std::slice::from_ref(&path), "127.0.0.1", 0).unwrap();
    let addr = server
        .url()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned();
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let handle = std::thread::spawn(move || server.run(|| stopped.try_recv().is_err()));

    let mut stalled = TcpStream::connect(&addr).unwrap();
    write!(stalled, "GET /big.pmtiles HTTP/1.0\r\nHost: {addr}\r\n\r\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert_eq!(get(&addr, "/big/9/3/0.mvt", "").0, 200);

    stop.send(()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !handle.is_finished() {
        assert!(std::time::Instant::now() < deadline, "server did not stop");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    handle.join().unwrap().unwrap();
    drop(stalled);
}

/// A directory entry claiming a body over the tile ceiling is refused
/// before the read allocates it, as `decode` refuses it (#510).
#[test]
fn an_oversized_tile_body_is_refused_before_the_read() {
    use tylertoo_core::internal::compression::{compress, MAX_TILE_BYTES};
    use tylertoo_core::internal::pmtiles_writer::{encode_directory, DirEntry, Header};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.pmtiles");
    write_archive(&path, 1);
    let mut out = std::fs::read(&path).unwrap();
    let mut header = Header::from_bytes(&out[..127]).unwrap();
    let body_len = MAX_TILE_BYTES + 1;
    let root = [DirEntry {
        tile_id: 0,
        offset: 0,
        length: u32::try_from(body_len).unwrap(),
        run_length: 1,
    }];
    let enc = compress(&encode_directory(&root), header.internal_compression).unwrap();
    header.root_dir_offset = out.len() as u64;
    header.root_dir_length = enc.len() as u64;
    out.extend_from_slice(&enc);
    header.tile_data_offset = out.len() as u64;
    header.tile_data_length = body_len;
    out[..127].copy_from_slice(&header.to_bytes());
    std::fs::write(&path, &out).unwrap();
    // A sparse tail: the body exists on disk without taking the space.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(out.len() as u64 + body_len)
        .unwrap();

    let server = Server::bind(std::slice::from_ref(&path), "127.0.0.1", 0).unwrap();
    let addr = server
        .url()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned();
    std::thread::spawn(move || server.run(|| true));
    let (code, _, body) = get(&addr, "/huge/0/0/0.mvt", "");
    assert_eq!(code, 500);
    assert!(String::from_utf8_lossy(&body).contains("tile ceiling"));
}

/// A file rewritten in place (same inode) that does not reopen is refused,
/// not read at the old directory's offsets; once it opens, it serves again.
#[test]
fn a_broken_in_place_rewrite_is_refused_until_it_reopens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inplace.pmtiles");
    write_archive(&path, 1);
    let server = Server::bind(std::slice::from_ref(&path), "127.0.0.1", 0).unwrap();
    let addr = server
        .url()
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_owned();
    std::thread::spawn(move || server.run(|| true));
    assert_eq!(get(&addr, "/inplace/0/0/0.mvt", "").0, 200);

    // `fs::write` truncates and rewrites the same inode.
    std::fs::write(&path, vec![0u8; 4096]).unwrap();
    assert_eq!(get(&addr, "/inplace/0/0/0.mvt", "").0, 503);

    let next = dir.path().join("next.pmtiles");
    write_archive(&next, 2);
    std::fs::write(&path, std::fs::read(&next).unwrap()).unwrap();
    let index = ArchiveIndex::open(&path).unwrap();
    let r = index.find(0, 0, 0).unwrap().unwrap();
    let (code, _, body) = get(&addr, "/inplace/0/0/0.mvt", "");
    assert_eq!((code, body), (200, index.read_range(r).unwrap()));
}
