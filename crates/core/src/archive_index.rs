//! Read a PMTiles v3 archive without loading it: header + directories only,
//! tile bodies fetched by offset on demand.
//!
//! The pyramid merge used to `std::fs::read` every band archive whole. That
//! is fine for a hand-built band and hopeless for a shard of a sharded build
//! (#498), where one input is tens of gigabytes — the merge would need the
//! whole shard set resident just to copy blobs between two files. An archive's
//! *directories*, though, stay small whatever the archive weighs: a dense
//! z0-z14 pyramid indexes ~3.6e8 tiles in a few megabytes of delta-encoded
//! varints, and #417's ceilings bound even a hostile one. So the index is read
//! eagerly and everything else is a positioned read.
//!
//! [`ArchiveIndex`] is that reader. It owns the `File`, the parsed
//! [`Header`], the fully expanded directory entries (root, with leaf
//! directories resolved inline), and the archive's raw metadata block — and
//! nothing else. Tiles come out through [`ArchiveIndex::tiles`] in ascending
//! tile-id order, as `(id, z, x, y, byte range)`; the bytes themselves are
//! read when a caller asks for them, so a merge can index several archives at
//! once and still touch each tile body exactly once.
//!
//! Every resource ceiling and error message the pyramid reader grew under
//! \#417 lives here now, so both readers — and any future one — inherit them
//! rather than re-deriving them.

use std::fs::File;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::compression::{self, MAX_INTERNAL_BYTES};
use crate::pmtiles_writer::{
    max_expanded_entries, read_all_entries_from, tile_id_to_zxy, ArchiveBytes, DirEntry, Header,
    HEADER_BYTES,
};
use crate::tile::TileBounds;
use crate::{Error, Result};

/// The bounds an archive actually claims, or `None`.
///
/// A writer that was never given bounds stores `TileBounds::empty()`, whose
/// infinities saturate to ±214.7° on the way into the header and read back
/// inverted (min > max). Unioning that swallows every real input, so an
/// unusable box is dropped rather than folded in. A zero-area box is kept: it
/// cannot poison a union, and a single-tile archive legitimately has one.
pub fn usable_bounds(header: &Header) -> Option<TileBounds> {
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

/// A `File` read by absolute offset, counting every byte it hands back.
///
/// The counter is what lets a test assert the "streaming" claim without
/// measuring RSS (flaky everywhere, hopeless in CI): after `open`, an
/// archive's `bytes_read` is its header plus its directories plus its
/// metadata, and nothing about the file's total size moves it.
struct FileBytes {
    file: File,
    len: u64,
    bytes_read: AtomicU64,
}

/// One positioned read, without disturbing any file cursor another reader
/// might hold.
///
/// `read_exact_at` (Unix) and `seek_read` (Windows) both take `&self` and are
/// atomic with respect to the file offset, so an `ArchiveIndex` is shareable.
/// The portable fallback seeks `&File` — `Read`/`Seek` are implemented for
/// `&File` — which is correct for one reader at a time and is only reached on
/// targets that have neither of the positioned APIs.
fn read_exact_at(file: &File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0usize;
        while done < buf.len() {
            // `seek_read` is a single positioned ReadFile: it may come back
            // short, so it is driven to completion like `read_exact` does.
            let n = file.seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "failed to fill whole buffer",
                ));
            }
            done += n;
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = file;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }
}

impl ArchiveBytes for FileBytes {
    fn total_len(&self) -> u64 {
        self.len
    }

    fn read_range(&self, offset: usize, len: usize) -> Result<std::borrow::Cow<'_, [u8]>> {
        let mut buf = vec![0u8; len];
        read_exact_at(&self.file, &mut buf, offset as u64)?;
        self.bytes_read.fetch_add(len as u64, Ordering::Relaxed);
        Ok(std::borrow::Cow::Owned(buf))
    }
}

/// One tile a [`TileIter`] found: its id, its coordinates, and where its
/// still-compressed body lives in the file.
///
/// The range is absolute (file offsets, not tile-data-section offsets), so it
/// can be handed straight to [`ArchiveIndex::read_range`] — or stashed and
/// read later, which is what a k-way merge across several archives does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TileRef {
    pub id: u64,
    pub z: u8,
    pub x: u32,
    pub y: u32,
    pub range: Range<usize>,
}

/// A PMTiles v3 archive opened for reading: header, directories and metadata
/// resident, tile bodies on demand.
///
/// See the module docs for why this is not a `std::fs::read`.
pub struct ArchiveIndex {
    src: FileBytes,
    path: PathBuf,
    header: Header,
    /// Root entries with every leaf pointer already resolved. Run lengths are
    /// left intact; expansion happens per tile in [`Self::tiles`].
    entries: Vec<DirEntry>,
    /// The archive's metadata block exactly as stored — still under
    /// `header.internal_compression`. Kept raw so a caller that wants a
    /// different parse than [`Self::metadata_json`] can have one.
    metadata_raw: Vec<u8>,
    bounds: Option<TileBounds>,
}

impl ArchiveIndex {
    /// Open `path`: header, directories and metadata, nothing else.
    ///
    /// Bytes read here are bounded by the archive's directory size, not by
    /// its total size — see [`Self::bytes_read`].
    pub fn open(path: &Path) -> Result<Self> {
        let at = |e: Error| Error::PMTilesWrite(format!("{}: {e}", path.display()));
        let file = File::open(path)
            .map_err(|e| Error::PMTilesRead(format!("failed to open {}: {e}", path.display())))?;
        let len = file
            .metadata()
            .map_err(|e| Error::PMTilesRead(format!("failed to stat {}: {e}", path.display())))?
            .len();
        let src = FileBytes {
            file,
            len,
            bytes_read: AtomicU64::new(0),
        };

        // The header is a fixed 127 bytes at offset 0; `Header::from_bytes`
        // validates magic, version and the fields #417 made load-bearing.
        let head = src
            .read_range(0, HEADER_BYTES.min(len as usize))
            .map_err(at)?;
        let header = Header::from_bytes(&head).map_err(at)?;

        let entries = read_all_entries_from(&src, &header).map_err(at)?;

        // Capped before the allocation, like the directory reads above
        // (#417/#510): `json_metadata_length` is archive-supplied and bounded
        // only by the file, so a header claiming 256 MiB of metadata used to
        // make `open` allocate that much — per input, and a merge opens every
        // shard at once — and then SUCCEED, holding it raw for the index's
        // lifetime. `metadata_json` decompresses it under the same ceiling, so
        // a larger compressed block cannot be legitimate metadata.
        if header.json_metadata_length > MAX_INTERNAL_BYTES {
            return Err(Error::PMTilesWrite(format!(
                "{}: metadata claims {} bytes, which exceeds the {MAX_INTERNAL_BYTES}-byte \
                 ceiling on an archive's internal sections",
                path.display(),
                header.json_metadata_length
            )));
        }
        let meta_end = header
            .json_metadata_offset
            .checked_add(header.json_metadata_length)
            .filter(|&e| e <= len)
            .ok_or_else(|| {
                Error::PMTilesWrite(format!("metadata past end of {}", path.display()))
            })?;
        let metadata_raw = src
            .read_range(
                header.json_metadata_offset as usize,
                (meta_end - header.json_metadata_offset) as usize,
            )
            .map_err(at)?
            .into_owned();

        let bounds = usable_bounds(&header);
        Ok(ArchiveIndex {
            src,
            path: path.to_path_buf(),
            header,
            entries,
            metadata_raw,
            bounds,
        })
    }

    /// The archive's path, for error messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The archive's parsed header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The directory entries, in ascending tile-id order, leaf directories
    /// already resolved inline. Run lengths are *not* expanded.
    pub fn entries(&self) -> &[DirEntry] {
        &self.entries
    }

    /// The metadata block as stored (still under
    /// `header().internal_compression`).
    pub fn metadata_raw(&self) -> &[u8] {
        &self.metadata_raw
    }

    /// The archive's header bounds, when they describe a real box — see
    /// [`usable_bounds`].
    pub fn bounds(&self) -> Option<TileBounds> {
        self.bounds
    }

    /// Total bytes this index has read from the file so far.
    ///
    /// After [`Self::open`] this is header + directories + metadata, which is
    /// the whole "streaming" claim in one number: a test can assert it
    /// against the file's size instead of trying to measure resident memory.
    /// It grows as tiles are fetched.
    pub fn bytes_read(&self) -> u64 {
        self.src.bytes_read.load(Ordering::Relaxed)
    }

    /// The archive's metadata parsed as JSON, or `None` when it is absent or
    /// unparseable (which is not fatal: the tiles are still copyable, they
    /// just describe no fields).
    pub fn metadata_json(&self) -> Option<serde_json::Value> {
        let plain = compression::decompress_capped(
            &self.metadata_raw,
            self.header.internal_compression,
            MAX_INTERNAL_BYTES,
        )
        .ok()?;
        serde_json::from_slice(&plain).ok()
    }

    /// Bytes at an absolute file range — a [`TileRef::range`], typically.
    pub fn read_range(&self, range: Range<usize>) -> Result<Vec<u8>> {
        if range.end > self.src.len as usize || range.start > range.end {
            return Err(Error::PMTilesWrite(format!(
                "tile data past end of {}",
                self.path.display()
            )));
        }
        Ok(self
            .src
            .read_range(range.start, range.end - range.start)?
            .into_owned())
    }

    /// Every addressed tile, in ascending tile-id order, run lengths expanded.
    ///
    /// Yields `Err` and then stops on the first corrupt entry; the ceilings
    /// are spent per directory entry (before any of its run is emitted), so a
    /// hostile run length is refused rather than half-walked.
    pub fn tiles(&self) -> TileIter<'_> {
        let limit = max_expanded_entries(&self.header);
        TileIter {
            archive: self,
            entry: 0,
            run: 0,
            run_len: 0,
            range: 0..0,
            limit,
            budget: limit,
            done: false,
        }
    }

    /// The archive's expanded tile-id range, `None` when it holds no tiles.
    ///
    /// Derived from the directory alone — no tile body is read — which is
    /// what makes the merge's disjointness check cheap enough to run on every
    /// input up front.
    pub fn tile_id_range(&self) -> Result<Option<(u64, u64)>> {
        // The same run-length expansion budget [`Self::tiles`] spends, for
        // the same reason and with the same wording (#417/#510): without it,
        // one hostile `run_length` came back as a plausible multi-billion-id
        // range, and the merge reported that as an *overlap* — blaming an
        // innocent archive for a corrupt one's directory.
        let limit = max_expanded_entries(&self.header);
        let mut budget = limit;
        let mut range: Option<(u64, u64)> = None;
        for e in &self.entries {
            let run = u64::from(e.run_length.max(1));
            match budget.checked_sub(run) {
                Some(b) => budget = b,
                None => {
                    return Err(Error::PMTilesWrite(format!(
                        "directory entry for tile id {} claims a run of {run} tiles, past this \
                         archive's run-length expansion limit of {limit} tiles",
                        e.tile_id
                    )))
                }
            }
            let last = e
                .tile_id
                .checked_add(run - 1)
                .ok_or_else(|| Error::PMTilesWrite("tile id past end of range".to_string()))?;
            range = Some(match range {
                None => (e.tile_id, last),
                Some((lo, hi)) => (lo.min(e.tile_id), hi.max(last)),
            });
        }
        Ok(range)
    }

    /// The archive's actual zoom range -- the lowest and highest zoom among
    /// tiles the directory addresses -- `None` when the archive holds no
    /// tiles at all.
    ///
    /// This is the archive's *actual* content, as opposed to
    /// `header().min_zoom..=header().max_zoom`. Between #380/#390 and
    /// #529/#522, `header()`'s own zoom range could also be a *declaration*
    /// widened over an empty zoom (`set_declared_min_zoom`/
    /// `set_declared_max_zoom`) so a client saw the range it was told to
    /// expect even where nothing was written -- but `go-pmtiles verify`
    /// rejects a header wider than the archive's actual tiles, so tylertoo's
    /// own writer no longer produces one; a declaration now only reaches
    /// `vector_layers[].minzoom`/`maxzoom`. An externally produced archive
    /// can still carry a genuinely mismatched header, though, so the pyramid
    /// band contract (#514) keys off this method rather than `header()`: a
    /// widened header can no longer make an empty band look like a
    /// legitimate subrange, and an honest, narrow header can no longer make a
    /// band that fully covers the archive's real tiles look like an error.
    ///
    /// Free of any run-length-expansion cost beyond [`Self::tile_id_range`]:
    /// tile ids are Hilbert-curve blocks per zoom, monotonic in zoom, so the
    /// lowest and highest id decode straight to the archive's actual min and
    /// max zoom.
    pub fn actual_zoom_range(&self) -> Result<Option<(u8, u8)>> {
        let Some((lo, hi)) = self.tile_id_range()? else {
            return Ok(None);
        };
        let (lo_z, _, _) = tile_id_to_zxy(lo)?;
        let (hi_z, _, _) = tile_id_to_zxy(hi)?;
        Ok(Some((lo_z, hi_z)))
    }
}

/// [`ArchiveIndex::tiles`]'s iterator: directory entries expanded into
/// individual tile ids, with #417's ceilings enforced as it walks.
pub struct TileIter<'a> {
    archive: &'a ArchiveIndex,
    /// Index of the directory entry currently being expanded.
    entry: usize,
    /// How much of that entry's run has been emitted.
    run: u64,
    run_len: u64,
    /// That entry's absolute byte range, resolved once per entry.
    range: Range<usize>,
    limit: u64,
    budget: u64,
    done: bool,
}

impl Iterator for TileIter<'_> {
    type Item = Result<TileRef>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // Offsets, lengths and ids are archive-supplied: every add is checked
        // so a corrupt directory reports rather than wraps.
        let past_end = || Error::PMTilesWrite("tile data past end of archive".to_string());
        let header = &self.archive.header;

        while self.run >= self.run_len {
            let Some(e) = self.archive.entries.get(self.entry) else {
                self.done = true;
                return None;
            };
            self.entry += 1;
            // Inside the declared tile-data section, not merely inside the
            // file: an entry aimed at the directories would otherwise be
            // handed to the MVT decoder as a tile (#417).
            match e.offset.checked_add(u64::from(e.length)) {
                Some(end) if end <= header.tile_data_length => {}
                _ => {
                    self.done = true;
                    return Some(Err(past_end()));
                }
            }
            let resolved = (|| {
                let start = header
                    .tile_data_offset
                    .checked_add(e.offset)
                    .and_then(|s| usize::try_from(s).ok())
                    .ok_or_else(past_end)?;
                let end = start
                    .checked_add(e.length as usize)
                    .filter(|&x| x as u64 <= self.archive.src.len)
                    .ok_or_else(past_end)?;
                Ok(start..end)
            })();
            match resolved {
                Ok(r) => self.range = r,
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
            // Run lengths are archive-controlled u32s, and every expanded id
            // becomes a map entry in a merge: one 0xFFFFFFFF run is tens of
            // gigabytes. Spend a budget of what this archive could
            // legitimately address, so many plausible runs cannot add up to
            // the same attack (#417). Spent before any of the run is emitted,
            // not after.
            let run = u64::from(e.run_length.max(1));
            let limit = self.limit;
            match self.budget.checked_sub(run) {
                Some(b) => self.budget = b,
                None => {
                    self.done = true;
                    return Some(Err(Error::PMTilesWrite(format!(
                        "directory entry for tile id {} claims a run of {run} tiles, past this \
                         archive's run-length expansion limit of {limit} tiles",
                        e.tile_id
                    ))));
                }
            }
            self.run = 0;
            self.run_len = run;
            // A zero-length run cannot happen (`max(1)`), but looping rather
            // than indexing keeps that an invariant of this code, not of the
            // archive's.
        }

        let e = &self.archive.entries[self.entry - 1];
        let i = self.run;
        self.run += 1;
        let id = match e.tile_id.checked_add(i) {
            Some(id) => id,
            None => {
                self.done = true;
                return Some(Err(Error::PMTilesWrite(
                    "tile id past end of range".to_string(),
                )));
            }
        };
        match tile_id_to_zxy(id) {
            Ok((z, x, y)) => Some(Ok(TileRef {
                id,
                z,
                x,
                y,
                range: self.range.clone(),
            })),
            Err(err) => {
                self.done = true;
                Some(Err(Error::PMTilesWrite(format!("bad tile id: {err}"))))
            }
        }
    }
}

/// Test-only: assert the `go-pmtiles verify` zoom invariants on an archive
/// (#529, #522). The header's `min_zoom`/`max_zoom` must equal the zooms the
/// directory actually addresses ([`ArchiveIndex::actual_zoom_range`]; z0..z0
/// for an archive with no tiles), and `center_zoom` must lie within them.
#[cfg(test)]
pub(crate) fn assert_header_zooms_match_tiles(path: &Path) {
    let idx = ArchiveIndex::open(path).expect("open archive");
    let h = idx.header();
    let actual = idx
        .actual_zoom_range()
        .expect("actual zoom range")
        .unwrap_or((0, 0));
    assert_eq!(
        (h.min_zoom, h.max_zoom),
        actual,
        "{}: header zoom range must equal the zooms that actually hold tiles \
         (go-pmtiles verify)",
        path.display()
    );
    assert!(
        h.min_zoom <= h.center_zoom && h.center_zoom <= h.max_zoom,
        "{}: center_zoom {} outside header z{}..z{}",
        path.display(),
        h.center_zoom,
        h.min_zoom,
        h.max_zoom
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compression::Compression;
    use crate::pmtiles_writer::StreamingPmtilesWriter;

    /// A modest archive, written the way every other producer here writes one.
    fn write_archive(path: &Path, tiles: &[(u8, u32, u32)], payload_len: usize) {
        let mut w = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
        w.set_layer_name("l");
        w.set_bounds(&TileBounds::new(-1.0, -1.0, 1.0, 1.0));
        for (i, (z, x, y)) in tiles.iter().enumerate() {
            // Distinct per tile, or the writer's dedup collapses them all and
            // there is nothing to seek between.
            let mut payload = vec![0u8; payload_len];
            payload[0] = i as u8;
            payload[1] = (i >> 8) as u8;
            w.add_tile(*z, *x, *y, &payload).unwrap();
        }
        w.finalize(path).unwrap();
    }

    /// The whole point of the type: opening an archive reads its header,
    /// directories and metadata — not its tile data — and scattered tiles
    /// still come back byte-identical to a full read of the file.
    #[test]
    fn archive_index_reads_tiles_without_loading_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pmtiles");
        let tiles: Vec<(u8, u32, u32)> = (0..64u32).map(|i| (6, i % 8, i / 8)).collect();
        // ~2 KiB of (incompressible) payload apiece, so tile data dwarfs the
        // directories by a wide enough margin that the assertion is not a
        // coin flip on gzip's mood.
        write_archive(&path, &tiles, 2048);

        let file_len = std::fs::metadata(&path).unwrap().len();
        let idx = ArchiveIndex::open(&path).unwrap();
        let after_open = idx.bytes_read();
        assert!(
            after_open < file_len / 4,
            "open read {after_open} of {file_len} bytes: that is not header + directories"
        );

        let refs: Vec<TileRef> = idx.tiles().map(|t| t.unwrap()).collect();
        assert_eq!(refs.len(), tiles.len());
        // Ascending tile id, which is what the merge's k-way ordering assumes.
        assert!(refs.windows(2).all(|w| w[0].id < w[1].id));

        // Scattered reads, against a baseline that slurps the file.
        let whole = std::fs::read(&path).unwrap();
        for i in [0usize, 7, 31, 63, 12] {
            let r = &refs[i];
            assert_eq!(
                idx.read_range(r.range.clone()).unwrap(),
                whole[r.range.clone()],
                "tile {}/{}/{} differs from the full-read baseline",
                r.z,
                r.x,
                r.y
            );
        }
        // Reading five tiles added five tiles' worth of bytes and no more.
        let fetched = idx.bytes_read() - after_open;
        let expected: usize = [0usize, 7, 31, 63, 12]
            .iter()
            .map(|&i| refs[i].range.len())
            .sum();
        assert_eq!(fetched, expected as u64);
    }

    /// #510 review, S3-4: [`ArchiveIndex::tiles`] spends a run-length
    /// expansion budget so a hostile `run_length` is refused, but
    /// [`ArchiveIndex::tile_id_range`] expanded the same runs with no budget
    /// at all. A single `0xFFFFFFFF` run therefore came back as a plausible
    /// four-billion-id range — which the merge then reported as an *overlap*,
    /// blaming an innocent shard for a corrupt one's directory. Same budget,
    /// same wording, so the diagnosis is "this archive is corrupt".
    #[test]
    fn tile_id_range_spends_the_run_length_budget() {
        use crate::pmtiles_writer::{encode_directory, DirEntry, Header, HEADER_BYTES};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pmtiles");
        write_archive(&path, &[(5, 0, 0), (5, 1, 1)], 32);

        let bytes = std::fs::read(&path).unwrap();
        let mut header = Header::from_bytes(&bytes).unwrap();
        let entries = [DirEntry {
            tile_id: 1,
            offset: 0,
            length: 8,
            run_length: u32::MAX,
        }];
        let enc = compression::compress(&encode_directory(&entries), header.internal_compression)
            .unwrap();
        let mut out = bytes.clone();
        header.root_dir_offset = out.len() as u64;
        header.root_dir_length = enc.len() as u64;
        out.extend_from_slice(&enc);
        out[..HEADER_BYTES].copy_from_slice(&header.to_bytes());
        let hostile = dir.path().join("hostile-run.pmtiles");
        std::fs::write(&hostile, &out).unwrap();

        let idx = ArchiveIndex::open(&hostile).unwrap();
        let err = idx
            .tile_id_range()
            .expect_err("a 4-billion-tile run must be refused, not reported as a range")
            .to_string();
        assert!(
            err.contains("run-length expansion"),
            "the message must diagnose corruption, not an overlap: {err}"
        );
    }

    /// The directory alone answers "which ids does this archive hold?", which
    /// is what makes the merge's disjointness check free.
    #[test]
    fn tile_id_range_comes_from_the_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pmtiles");
        write_archive(&path, &[(3, 0, 0), (3, 1, 1), (3, 7, 7)], 32);

        let idx = ArchiveIndex::open(&path).unwrap();
        let before = idx.bytes_read();
        let (lo, hi) = idx.tile_id_range().unwrap().unwrap();
        assert_eq!(idx.bytes_read(), before, "the range must read no bytes");

        let ids: Vec<u64> = idx.tiles().map(|t| t.unwrap().id).collect();
        assert_eq!(lo, *ids.iter().min().unwrap());
        assert_eq!(hi, *ids.iter().max().unwrap());
    }

    /// #514: the pyramid band contract needs the archive's *actual* zoom
    /// range, not its (possibly widened) declared header range. This decodes
    /// straight from [`ArchiveIndex::tile_id_range`], with no extra cost.
    #[test]
    fn actual_zoom_range_spans_the_lowest_and_highest_tile_zoom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.pmtiles");
        write_archive(&path, &[(5, 0, 0), (3, 1, 1), (7, 2, 2)], 32);

        let idx = ArchiveIndex::open(&path).unwrap();
        assert_eq!(idx.actual_zoom_range().unwrap(), Some((3, 7)));
    }

    /// An archive with no tiles at all has no actual zoom range -- `None`,
    /// not `(0, 0)`, so a caller cannot mistake "empty" for "z0 only".
    #[test]
    fn actual_zoom_range_is_none_for_an_empty_archive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.pmtiles");
        write_archive(&path, &[], 32);

        let idx = ArchiveIndex::open(&path).unwrap();
        assert_eq!(idx.actual_zoom_range().unwrap(), None);
    }
}
