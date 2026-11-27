//! A PMTiles archive baked into a GeoParquet file (`geo:pmtiles`).
//!
//! ```text
//! PAR1 | row groups, page indexes (bytes unchanged) | PMTiles v3 archive | footer' | len | PAR1
//! footer' = the original FileMetaData, with one last key_value_metadata entry
//!   geo:pmtiles = {"spec":"0.1.0","offset":<archive start>,"length":<archive bytes>}
//! ```
//!
//! The archive goes where the footer was, so no offset the footer holds
//! moves, and a Parquet reader that follows the footer sees the original
//! GeoParquet. Only `key_value_metadata` is rebuilt; every other footer byte
//! is copied ([`tylertoo_io::FooterKeyValues::rewrite`]), so [`strip`] gives
//! back the original file. PMTiles offsets are relative to the archive's
//! start, so the archive is copied unchanged and read at `offset + n`.
//!
//! This module owns the convention: [`bake`], [`strip`], and [`locate_baked`],
//! which every PMTiles reader in the crate goes through
//! (`archive_index::FileBytes`).

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::pmtiles_writer::{Header, HEADER_BYTES};

/// The footer key of a baked archive.
pub const BAKED_KEY: &str = "geo:pmtiles";
/// The version of the `geo:pmtiles` convention this crate writes.
pub const BAKED_SPEC: &str = "0.1.0";

const MAGIC: &[u8; 4] = b"PAR1";
/// `PAR1` at the start, footer length and `PAR1` at the end.
const TRAILER: u64 = 8;

/// Where a baked archive sits in its GeoParquet file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BakedArchive {
    /// The archive's first byte, from the start of the file.
    pub offset: u64,
    /// The archive's size in bytes.
    pub length: u64,
}

/// What [`bake`] or [`strip`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct BakeReport {
    /// The archive baked in ([`bake`]) or removed ([`strip`]).
    pub archive: BakedArchive,
    /// The output file's size in bytes.
    pub output_bytes: u64,
}

/// Why a file could not be baked, stripped, or read as a baked archive.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BakeError {
    /// A file could not be opened, read or written.
    #[error("{}: {error}", path.display())]
    #[non_exhaustive]
    Io {
        /// The file.
        path: PathBuf,
        /// The I/O error.
        error: io::Error,
    },
    /// The output cannot be written (checked before any input is read).
    #[error(transparent)]
    Output(#[from] crate::atomic_output::OutputNotWritable),
    /// The file is not Parquet: it lacks the `PAR1` magic at either end.
    #[error("{} is not a Parquet file", path.display())]
    #[non_exhaustive]
    NotParquet {
        /// The file.
        path: PathBuf,
    },
    /// The file's footer is malformed.
    #[error("{}: {reason}", path.display())]
    #[non_exhaustive]
    Footer {
        /// The file.
        path: PathBuf,
        /// What is wrong with it.
        reason: String,
    },
    /// The file is Parquet but not GeoParquet: its footer has no `geo` key.
    #[error("{} is not GeoParquet: its footer has no \"geo\" metadata", path.display())]
    #[non_exhaustive]
    NotGeoParquet {
        /// The file.
        path: PathBuf,
    },
    /// The file already holds a baked archive.
    #[error(
        "{} already holds a baked PMTiles archive; strip it first with `tylertoo bake --strip`",
        path.display()
    )]
    #[non_exhaustive]
    AlreadyBaked {
        /// The file.
        path: PathBuf,
    },
    /// The file holds no baked archive.
    #[error("{} holds no baked PMTiles archive (no \"{BAKED_KEY}\" footer entry)", path.display())]
    #[non_exhaustive]
    NotBaked {
        /// The file.
        path: PathBuf,
    },
    /// The `geo:pmtiles` entry does not point at a PMTiles archive inside
    /// the file, as when a tool rewrote the data and copied the footer
    /// metadata.
    #[error(
        "{}: the \"{BAKED_KEY}\" footer entry does not point at a PMTiles archive ({reason}); \
         the file was probably rewritten after it was baked",
        path.display()
    )]
    #[non_exhaustive]
    StaleEntry {
        /// The file.
        path: PathBuf,
        /// What does not hold.
        reason: String,
    },
    /// The archive to bake is not a readable PMTiles v3 archive.
    #[error(transparent)]
    Archive(#[from] crate::Error),
}

impl crate::error::Classify for BakeError {
    /// A file that is not GeoParquet, is baked already or not at all, or
    /// carries a stale entry is the caller's input; a failed read or write
    /// is a failed run.
    fn kind(&self) -> crate::error::ErrorKind {
        use crate::error::ErrorKind;
        match self {
            BakeError::Io { .. } | BakeError::Output(_) => ErrorKind::Runtime,
            BakeError::Archive(e) => e.kind(),
            _ => ErrorKind::Input,
        }
    }
}

/// A Parquet file's footer: its thrift bytes and where they start.
struct Footer {
    thrift: Vec<u8>,
    start: u64,
}

/// Read the footer of a Parquet file of `len` bytes through `read_at`.
fn read_footer(
    path: &Path,
    len: u64,
    read_at: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> Result<Footer, BakeError> {
    let io = |error| BakeError::Io {
        path: path.to_path_buf(),
        error,
    };
    let not_parquet = || BakeError::NotParquet {
        path: path.to_path_buf(),
    };
    if len < 4 + TRAILER {
        return Err(not_parquet());
    }
    let mut head = [0u8; 4];
    read_at(0, &mut head).map_err(io)?;
    let mut tail = [0u8; 8];
    read_at(len - TRAILER, &mut tail).map_err(io)?;
    if &head != MAGIC || &tail[4..] != MAGIC {
        return Err(not_parquet());
    }
    let footer_len = u64::from(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
    let start = (len - TRAILER)
        .checked_sub(footer_len)
        .filter(|&s| s >= 4)
        .ok_or_else(|| BakeError::Footer {
            path: path.to_path_buf(),
            reason: format!(
                "the trailer declares a {footer_len}-byte footer, past the file's start"
            ),
        })?;
    let mut thrift = vec![0u8; footer_len as usize];
    read_at(start, &mut thrift).map_err(io)?;
    Ok(Footer { thrift, start })
}

fn key_values(path: &Path, thrift: &[u8]) -> Result<tylertoo_io::FooterKeyValues, BakeError> {
    tylertoo_io::footer_key_values(thrift)
        .map_err(|e| BakeError::Footer {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?
        .ok_or_else(|| BakeError::NotGeoParquet {
            path: path.to_path_buf(),
        })
}

/// The archive the `geo:pmtiles` entry `value` points at, checked against
/// the file: inside `[4, footer start)`, starting with a PMTiles v3 header
/// whose sections fit inside the declared length.
fn check_entry(
    path: &Path,
    value: &[u8],
    footer_start: u64,
    read_at: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> Result<BakedArchive, BakeError> {
    let stale = |reason: String| BakeError::StaleEntry {
        path: path.to_path_buf(),
        reason,
    };
    let json: serde_json::Value =
        serde_json::from_slice(value).map_err(|e| stale(format!("not JSON: {e}")))?;
    let field = |name: &str| {
        json.get(name)
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| stale(format!("no integer \"{name}\"")))
    };
    let (offset, length) = (field("offset")?, field("length")?);
    if offset < 4
        || offset
            .checked_add(length)
            .is_none_or(|end| end > footer_start)
    {
        return Err(stale(format!(
            "bytes {offset}+{length} are not between the data's start and the footer at {footer_start}"
        )));
    }
    if length < HEADER_BYTES as u64 {
        return Err(stale(format!(
            "{length} bytes cannot hold a PMTiles header"
        )));
    }
    let mut head = [0u8; HEADER_BYTES];
    read_at(offset, &mut head).map_err(|error| BakeError::Io {
        path: path.to_path_buf(),
        error,
    })?;
    let header = Header::from_bytes(&head).map_err(|e| stale(e.to_string()))?;
    let sections = [
        (
            "root directory",
            header.root_dir_offset,
            header.root_dir_length,
        ),
        (
            "metadata",
            header.json_metadata_offset,
            header.json_metadata_length,
        ),
        (
            "leaf directories",
            header.leaf_dirs_offset,
            header.leaf_dirs_length,
        ),
        (
            "tile data",
            header.tile_data_offset,
            header.tile_data_length,
        ),
    ];
    for (what, at, len) in sections {
        if at.checked_add(len).is_none_or(|e| e > length) {
            return Err(stale(format!(
                "the archive's {what} runs past its {length} bytes"
            )));
        }
    }
    Ok(BakedArchive { offset, length })
}

/// Where the baked archive of the Parquet file of `len` bytes read through
/// `read_at` sits: `None` when the footer has no `geo:pmtiles` entry.
pub(crate) fn locate_with(
    path: &Path,
    len: u64,
    read_at: &mut dyn FnMut(u64, &mut [u8]) -> io::Result<()>,
) -> Result<Option<BakedArchive>, BakeError> {
    let footer = read_footer(path, len, read_at)?;
    let Some(kv) =
        tylertoo_io::footer_key_values(&footer.thrift).map_err(|e| BakeError::Footer {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?
    else {
        return Ok(None);
    };
    kv.get(BAKED_KEY)
        .map(|value| check_entry(path, value, footer.start, read_at))
        .transpose()
}

fn open(path: &Path) -> Result<(File, u64), BakeError> {
    let io = |error| BakeError::Io {
        path: path.to_path_buf(),
        error,
    };
    let file = File::open(path).map_err(io)?;
    let len = file.metadata().map_err(io)?.len();
    Ok((file, len))
}

/// A positioned reader over `file`, for [`locate_with`].
fn reader(file: &File) -> impl FnMut(u64, &mut [u8]) -> io::Result<()> + '_ {
    move |offset, buf| {
        let mut f = file;
        f.seek(SeekFrom::Start(offset))?;
        f.read_exact(buf)
    }
}

/// Where the archive baked into the GeoParquet file at `path` sits, or
/// `None` when the file holds none.
///
/// # Errors
///
/// Returns [`BakeError::NotParquet`] for a file that is not Parquet,
/// [`BakeError::Footer`] for a malformed footer, and
/// [`BakeError::StaleEntry`] when the `geo:pmtiles` entry does not point at
/// a PMTiles archive inside the file.
pub fn locate_baked(path: impl AsRef<Path>) -> Result<Option<BakedArchive>, BakeError> {
    let path = path.as_ref();
    let (file, len) = open(path)?;
    let mut read_at = reader(&file);
    locate_with(path, len, &mut read_at)
}

/// Copy `n` bytes of `src` from its current position into `dst`.
fn copy_exact(src: &mut File, n: u64, dst: &mut impl Write, path: &Path) -> Result<(), BakeError> {
    let copied = io::copy(&mut src.take(n), dst).map_err(|error| BakeError::Io {
        path: path.to_path_buf(),
        error,
    })?;
    if copied != n {
        return Err(BakeError::Io {
            path: path.to_path_buf(),
            error: io::Error::new(io::ErrorKind::UnexpectedEof, "the file shrank while read"),
        });
    }
    Ok(())
}

/// Write `prefix` bytes of `src`, then `middle`, then `thrift` and the
/// Parquet trailer, to `output`, atomically.
fn write_output(
    output: &Path,
    src: (&mut File, &Path, u64),
    middle: Option<(&mut File, &Path, u64)>,
    thrift: &[u8],
) -> Result<u64, BakeError> {
    let out_io = |error| BakeError::Io {
        path: output.to_path_buf(),
        error,
    };
    let footer_len = u32::try_from(thrift.len()).map_err(|_| BakeError::Footer {
        path: output.to_path_buf(),
        reason: "the rewritten footer exceeds 4 GiB".into(),
    })?;
    let (file, pending) = crate::atomic_output::create(output).map_err(out_io)?;
    let mut w = io::BufWriter::with_capacity(1 << 20, file);
    let (src, src_path, prefix) = src;
    src.seek(SeekFrom::Start(0))
        .map_err(|error| BakeError::Io {
            path: src_path.to_path_buf(),
            error,
        })?;
    copy_exact(src, prefix, &mut w, src_path)?;
    if let Some((archive, archive_path, len)) = middle {
        archive
            .seek(SeekFrom::Start(0))
            .map_err(|error| BakeError::Io {
                path: archive_path.to_path_buf(),
                error,
            })?;
        copy_exact(archive, len, &mut w, archive_path)?;
    }
    w.write_all(thrift).map_err(out_io)?;
    w.write_all(&footer_len.to_le_bytes()).map_err(out_io)?;
    w.write_all(MAGIC).map_err(out_io)?;
    let file = w.into_inner().map_err(|e| out_io(e.into_error()))?;
    file.sync_all().map_err(out_io)?;
    let written = file.metadata().map_err(out_io)?.len();
    drop(file);
    pending.publish().map_err(out_io)?;
    Ok(written)
}

/// Bake the PMTiles archive at `archive` into a copy of `geoparquet`.
///
/// The copy, written to `output`, is the input's bytes up to its footer,
/// then the archive, then the input's footer with a `geo:pmtiles` entry
/// appended. A stale `geo:pmtiles` entry (one that points at no archive) is
/// replaced.
///
/// # Errors
///
/// Returns an error if `output` cannot be written, if `geoparquet` is not
/// GeoParquet or already holds a baked archive, or if `archive` is not a
/// readable PMTiles v3 archive.
///
/// # Example
///
/// ```no_run
/// let report = tylertoo_core::bake("parcels.parquet", "parcels.pmtiles", "baked.parquet")?;
/// println!("archive at byte {}", report.archive.offset);
/// # Ok::<(), tylertoo_core::BakeError>(())
/// ```
pub fn bake(
    geoparquet: impl AsRef<Path>,
    archive: impl AsRef<Path>,
    output: impl AsRef<Path>,
) -> Result<BakeReport, BakeError> {
    let (geoparquet, archive, output) = (geoparquet.as_ref(), archive.as_ref(), output.as_ref());
    crate::atomic_output::check_writable(output)?;
    // The whole archive is validated (header, directories, metadata), not
    // just its magic: a baked file is only as good as what it carries.
    crate::archive_index::ArchiveIndex::open(archive)?;
    let (mut src, len) = open(geoparquet)?;
    let footer = read_footer(geoparquet, len, &mut reader(&src))?;
    let kv = key_values(geoparquet, &footer.thrift)?;
    if kv.get("geo").is_none() {
        return Err(BakeError::NotGeoParquet {
            path: geoparquet.to_path_buf(),
        });
    }
    if let Some(value) = kv.get(BAKED_KEY) {
        match check_entry(geoparquet, value, footer.start, &mut reader(&src)) {
            Ok(_) => {
                return Err(BakeError::AlreadyBaked {
                    path: geoparquet.to_path_buf(),
                })
            }
            Err(BakeError::StaleEntry { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    let (mut tiles, archive_len) = open(archive)?;
    let baked = BakedArchive {
        offset: footer.start,
        length: archive_len,
    };
    let value = serde_json::json!({
        "spec": BAKED_SPEC,
        "offset": baked.offset,
        "length": baked.length,
    })
    .to_string();
    let thrift = kv.rewrite(
        &footer.thrift,
        |e| e.key != BAKED_KEY,
        &[(BAKED_KEY, value.as_bytes())],
    );
    let output_bytes = write_output(
        output,
        (&mut src, geoparquet, footer.start),
        Some((&mut tiles, archive, archive_len)),
        &thrift,
    )?;
    Ok(BakeReport {
        archive: baked,
        output_bytes,
    })
}

/// Write the GeoParquet file at `baked` without its baked archive to `output`.
///
/// The output is the bytes before the archive, then the footer without its
/// `geo:pmtiles` entry: for a file [`bake`] wrote, the original file, byte
/// for byte.
///
/// # Errors
///
/// Returns an error if `output` cannot be written, if `baked` holds no
/// baked archive, or if its `geo:pmtiles` entry is stale.
///
/// # Example
///
/// ```no_run
/// tylertoo_core::strip("baked.parquet", "plain.parquet")?;
/// # Ok::<(), tylertoo_core::BakeError>(())
/// ```
pub fn strip(baked: impl AsRef<Path>, output: impl AsRef<Path>) -> Result<BakeReport, BakeError> {
    let (baked, output) = (baked.as_ref(), output.as_ref());
    crate::atomic_output::check_writable(output)?;
    let (mut src, len) = open(baked)?;
    let footer = read_footer(baked, len, &mut reader(&src))?;
    let kv = key_values(baked, &footer.thrift)?;
    let value = kv.get(BAKED_KEY).ok_or_else(|| BakeError::NotBaked {
        path: baked.to_path_buf(),
    })?;
    let archive = check_entry(baked, value, footer.start, &mut reader(&src))?;
    // The data must end where the archive starts: anything else between
    // the archive and the footer would be cut off with it.
    if archive.offset + archive.length != footer.start {
        return Err(BakeError::StaleEntry {
            path: baked.to_path_buf(),
            reason: format!(
                "the archive ends at byte {}, not at the footer ({})",
                archive.offset + archive.length,
                footer.start
            ),
        });
    }
    let thrift = kv.rewrite(&footer.thrift, |e| e.key != BAKED_KEY, &[]);
    let output_bytes = write_output(output, (&mut src, baked, archive.offset), None, &thrift)?;
    Ok(BakeReport {
        archive,
        output_bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A GeoParquet file of `n` points.
    fn geoparquet(dir: &Path, n: usize) -> PathBuf {
        let path = dir.join("points.parquet");
        let geoms: Vec<Option<geo::Geometry<f64>>> = (0..n)
            .map(|i| {
                Some(geo::Geometry::Point(geo::Point::new(
                    -74.0 + (i % 40) as f64 * 0.01,
                    40.6 + (i / 40) as f64 * 0.01,
                )))
            })
            .collect();
        crate::overview::testutil::write_input(&path, &geoms, false, None);
        path
    }

    /// A small PMTiles archive of point tiles at zoom 5.
    fn archive(dir: &Path) -> PathBuf {
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        // One point at (1, 1) in layer "t", extent 4096.
        const POINT_TILE: &[u8] = &[
            0x1a, 0x0f, 0x78, 0x02, 0x0a, 0x01, 0x74, 0x28, 0x80, 0x20, 0x12, 0x03, 0x09, 0x02,
            0x02,
        ];
        let path = dir.join("tiles.pmtiles");
        let mut writer = StreamingPmtilesWriter::new(crate::Compression::Gzip).unwrap();
        writer.set_layer_name("t");
        for i in 0..8 {
            writer.add_tile(5, i, 7, POINT_TILE).unwrap();
        }
        writer.finalize(&path).unwrap();
        path
    }

    fn keys(path: &Path) -> Vec<String> {
        let bytes = std::fs::read(path).unwrap();
        let n = bytes.len();
        let len = u32::from_le_bytes(bytes[n - 8..n - 4].try_into().unwrap()) as usize;
        tylertoo_io::footer_key_values(&bytes[n - 8 - len..n - 8])
            .unwrap()
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.key)
            .collect()
    }

    #[test]
    fn bake_then_strip_gives_back_the_original_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (gpq, tiles) = (geoparquet(dir.path(), 200), archive(dir.path()));
        let baked = dir.path().join("baked.parquet");
        let report = bake(&gpq, &tiles, &baked).unwrap();
        let original = std::fs::read(&gpq).unwrap();
        let archive_bytes = std::fs::read(&tiles).unwrap();
        let out = std::fs::read(&baked).unwrap();
        assert_eq!(report.output_bytes, out.len() as u64);
        // The data before the archive is the original's, byte for byte.
        let at = report.archive.offset as usize;
        assert_eq!(&out[..at], &original[..at]);
        assert_eq!(&out[at..at + archive_bytes.len()], &archive_bytes[..]);
        assert_eq!(keys(&baked).last().map(String::as_str), Some(BAKED_KEY));
        assert_eq!(locate_baked(&baked).unwrap(), Some(report.archive));

        let plain = dir.path().join("plain.parquet");
        strip(&baked, &plain).unwrap();
        assert_eq!(std::fs::read(&plain).unwrap(), original);
        assert_eq!(locate_baked(&plain).unwrap(), None);
    }

    #[test]
    fn a_baked_file_reads_as_the_original_geoparquet() {
        let dir = tempfile::tempdir().unwrap();
        let (gpq, tiles) = (geoparquet(dir.path(), 500), archive(dir.path()));
        let baked = dir.path().join("baked.parquet");
        bake(&gpq, &tiles, &baked).unwrap();
        let rows = |p: &Path| -> usize {
            let source = crate::input_set::ConvertSource::resolve_path(p).unwrap();
            let plan = crate::input_set::ReadPlan {
                batch_size: 1 << 16,
                projection: None,
                row_groups: None,
            };
            source
                .open_stream(&plan)
                .unwrap()
                .map(|b| b.unwrap().num_rows())
                .sum()
        };
        assert_eq!(rows(&baked), 500);
        assert_eq!(rows(&baked), rows(&gpq));
    }

    #[test]
    fn the_archive_reader_opens_a_baked_file() {
        let dir = tempfile::tempdir().unwrap();
        let (gpq, tiles) = (geoparquet(dir.path(), 10), archive(dir.path()));
        let baked = dir.path().join("baked.parquet");
        bake(&gpq, &tiles, &baked).unwrap();
        let plain = crate::archive_index::ArchiveIndex::open(&tiles).unwrap();
        let inside = crate::archive_index::ArchiveIndex::open(&baked).unwrap();
        assert_eq!(inside.entries().len(), plain.entries().len());
        assert_eq!(inside.metadata_json(), plain.metadata_json());
        let a: Vec<_> = plain
            .tiles()
            .map(|t| plain.read_range(t.unwrap().range).unwrap())
            .collect();
        let b: Vec<_> = inside
            .tiles()
            .map(|t| inside.read_range(t.unwrap().range).unwrap())
            .collect();
        assert_eq!(a, b);
        assert_eq!(a.len(), 8);
    }

    #[test]
    fn the_archive_reader_refuses_plain_geoparquet_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let gpq = geoparquet(dir.path(), 10);
        let err = crate::archive_index::ArchiveIndex::open(&gpq)
            .err()
            .unwrap();
        assert!(err.to_string().contains(BAKED_KEY), "{err}");
    }

    #[test]
    fn baking_twice_is_refused_and_plain_parquet_is_not_geoparquet() {
        let dir = tempfile::tempdir().unwrap();
        let (gpq, tiles) = (geoparquet(dir.path(), 10), archive(dir.path()));
        let baked = dir.path().join("baked.parquet");
        bake(&gpq, &tiles, &baked).unwrap();
        let again = dir.path().join("again.parquet");
        assert!(matches!(
            bake(&baked, &tiles, &again),
            Err(BakeError::AlreadyBaked { .. })
        ));
        assert!(matches!(
            strip(&gpq, &again),
            Err(BakeError::NotBaked { .. })
        ));
        assert!(matches!(
            bake(&tiles, &tiles, &again),
            Err(BakeError::NotParquet { .. })
        ));
        assert!(!again.exists());
    }

    #[test]
    fn a_stale_entry_is_reported_and_replaced_by_a_new_bake() {
        let dir = tempfile::tempdir().unwrap();
        let (gpq, tiles) = (geoparquet(dir.path(), 10), archive(dir.path()));
        // A rewrite that kept the footer metadata: the entry points into
        // the data, where no archive is.
        let bytes = std::fs::read(&gpq).unwrap();
        let n = bytes.len();
        let len = u32::from_le_bytes(bytes[n - 8..n - 4].try_into().unwrap()) as usize;
        let thrift = &bytes[n - 8 - len..n - 8];
        let kv = tylertoo_io::footer_key_values(thrift).unwrap().unwrap();
        let entry = format!(
            "{{\"spec\":\"0.1.0\",\"offset\":4,\"length\":{}}}",
            HEADER_BYTES
        );
        let footer = kv.rewrite(thrift, |_| true, &[(BAKED_KEY, entry.as_bytes())]);
        let mut stale = bytes[..n - 8 - len].to_vec();
        stale.extend_from_slice(&footer);
        stale.extend_from_slice(&(footer.len() as u32).to_le_bytes());
        stale.extend_from_slice(b"PAR1");
        let stale_path = dir.path().join("stale.parquet");
        std::fs::write(&stale_path, &stale).unwrap();

        assert!(matches!(
            locate_baked(&stale_path),
            Err(BakeError::StaleEntry { .. })
        ));
        let rebaked = dir.path().join("rebaked.parquet");
        let report = bake(&stale_path, &tiles, &rebaked).unwrap();
        assert_eq!(locate_baked(&rebaked).unwrap(), Some(report.archive));
        let ks = keys(&rebaked);
        assert_eq!(ks.iter().filter(|k| *k == BAKED_KEY).count(), 1);
    }

    #[test]
    fn entries_outside_the_file_or_not_pointing_at_pmtiles_are_stale() {
        let dir = tempfile::tempdir().unwrap();
        let (gpq, tiles) = (geoparquet(dir.path(), 10), archive(dir.path()));
        let baked = dir.path().join("baked.parquet");
        let report = bake(&gpq, &tiles, &baked).unwrap();
        let (file, len) = open(&baked).unwrap();
        let footer = read_footer(&baked, len, &mut reader(&file)).unwrap();
        let check = |v: &str| check_entry(&baked, v.as_bytes(), footer.start, &mut reader(&file));
        let a = report.archive;
        assert_eq!(
            check(&format!(
                "{{\"offset\":{},\"length\":{}}}",
                a.offset, a.length
            ))
            .unwrap(),
            a
        );
        for bad in [
            "not json".to_owned(),
            "{\"offset\":4}".to_owned(),
            format!("{{\"offset\":{},\"length\":{}}}", a.offset, a.length + 1),
            format!("{{\"offset\":0,\"length\":{}}}", a.length),
            format!(
                "{{\"offset\":{},\"length\":{}}}",
                a.offset + 1,
                a.length - 1
            ),
            format!("{{\"offset\":{},\"length\":{}}}", u64::MAX, 2),
            format!("{{\"offset\":{},\"length\":100}}", a.offset),
        ] {
            assert!(
                matches!(check(&bad), Err(BakeError::StaleEntry { .. })),
                "{bad}"
            );
        }
    }
}
