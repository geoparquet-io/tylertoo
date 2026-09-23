//! Compression support for PMTiles v3.
//!
//! PMTiles supports multiple compression algorithms:
//! - None (1): No compression
//! - Gzip (2): zlib/gzip compression (default)
//! - Brotli (3): Brotli compression (good for web)
//! - Zstd (4): Zstandard compression (fast, high ratio)
//!
//! This module provides a unified interface for compressing tile and directory data.

use std::io::{self, Write};

/// Compression algorithm for PMTiles.
///
/// Values match the PMTiles v3 spec:
/// - 0: Unknown
/// - 1: None
/// - 2: Gzip
/// - 3: Brotli
/// - 4: Zstd
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Compression {
    Unknown = 0,
    None = 1,
    #[default]
    Gzip = 2,
    Brotli = 3,
    Zstd = 4,
}

impl Compression {
    /// Parse compression from string (case-insensitive).
    ///
    /// Valid values: "none", "gzip", "brotli", "zstd"
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "none" => Some(Compression::None),
            "gzip" => Some(Compression::Gzip),
            "brotli" => Some(Compression::Brotli),
            "zstd" => Some(Compression::Zstd),
            _ => Option::None,
        }
    }

    /// Get the PMTiles byte code for this compression type.
    pub fn code(&self) -> u8 {
        *self as u8
    }

    /// Parse a PMTiles spec byte code back into a compression type.
    ///
    /// Inverse of [`Compression::code`]. Returns `None` for codes outside the
    /// PMTiles v3 spec (0-4).
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Compression::Unknown),
            1 => Some(Compression::None),
            2 => Some(Compression::Gzip),
            3 => Some(Compression::Brotli),
            4 => Some(Compression::Zstd),
            _ => Option::None,
        }
    }

    /// Get a human-readable name for this compression type.
    pub fn name(&self) -> &'static str {
        match self {
            Compression::Unknown => "unknown",
            Compression::None => "none",
            Compression::Gzip => "gzip",
            Compression::Brotli => "brotli",
            Compression::Zstd => "zstd",
        }
    }
}

/// Ceiling for decompressing one PMTiles *internal* section: a root or leaf
/// directory, or the JSON metadata (#417).
///
/// Directories are sized for the 16 KiB initial range request — the writer
/// caps the compressed root at `MAX_ROOT_DIR_BYTES` (16257 bytes) and
/// partitions the rest into leaves of a few thousand entries, each entry
/// about 30 bytes of varints once decompressed. 16 MiB leaves room for
/// roughly half a million entries in a single directory: orders of magnitude
/// past any directory (or TileJSON metadata blob) a real archive carries,
/// while keeping a decompression bomb to a bounded allocation.
pub const MAX_INTERNAL_BYTES: u64 = 16 * 1024 * 1024;

/// Ceiling for decompressing one tile body (#417).
///
/// Our exporter caps encoded tiles at `DEFAULT_TILE_SIZE_LIMIT` (500 KiB —
/// tippecanoe's default bar). A foreign archive may have been written with
/// that cap disabled, so this leaves 1000x headroom; what it forbids is the
/// KB-sized bomb that expands to tens of gigabytes.
pub const MAX_TILE_BYTES: u64 = 500 * 1024 * 1024;

/// One direction of the codec: the three per-algorithm entry points, the
/// uncompressed passthrough, and the verb used in the `Unknown` error
/// message.
///
/// `compress` and `decompress` are mirror images. Writing the match arms twice
/// meant every new algorithm had to be added in two places, so the dispatch
/// lives here once and each public function supplies its own table. `A` is the
/// extra argument a direction needs: nothing for compression, the output
/// ceiling for decompression.
struct Codec<A> {
    verb: &'static str,
    none: fn(&[u8], A) -> io::Result<Vec<u8>>,
    gzip: fn(&[u8], A) -> io::Result<Vec<u8>>,
    brotli: fn(&[u8], A) -> io::Result<Vec<u8>>,
    zstd: fn(&[u8], A) -> io::Result<Vec<u8>>,
}

const COMPRESS: &Codec<()> = &Codec {
    verb: "compress",
    none: copy_all,
    gzip: compress_gzip,
    brotli: compress_brotli,
    zstd: compress_zstd,
};

const DECOMPRESS: &Codec<u64> = &Codec {
    verb: "decompress",
    none: copy_capped,
    gzip: decompress_gzip,
    brotli: decompress_brotli,
    zstd: decompress_zstd,
};

fn dispatch<A>(
    data: &[u8],
    compression: Compression,
    arg: A,
    codec: &Codec<A>,
) -> io::Result<Vec<u8>> {
    match compression {
        Compression::Unknown => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Cannot {} with unknown compression type", codec.verb),
        )),
        Compression::None => (codec.none)(data, arg),
        Compression::Gzip => (codec.gzip)(data, arg),
        Compression::Brotli => (codec.brotli)(data, arg),
        Compression::Zstd => (codec.zstd)(data, arg),
    }
}

/// Compress data using the specified algorithm.
///
/// # Arguments
/// * `data` - Uncompressed input data
/// * `compression` - Compression algorithm to use
///
/// # Returns
/// Compressed data, or original data if compression is None.
pub fn compress(data: &[u8], compression: Compression) -> io::Result<Vec<u8>> {
    dispatch(data, compression, (), COMPRESS)
}

/// Decompress data using the specified algorithm.
///
/// Inverse of [`compress`]: the read side of the PMTiles pipeline (issue
/// #112) uses this for directories, JSON metadata, and tile data, honoring
/// the compression codes declared in the archive header.
///
/// Every input is archive-controlled, so decompression is *bounded*: a
/// KB-sized bomb must not become tens of gigabytes of resident memory
/// (#417). `max_out` is the exact number of decompressed bytes the caller is
/// willing to hold — [`MAX_INTERNAL_BYTES`] for directories and metadata,
/// [`MAX_TILE_BYTES`] for a tile body. Output *of exactly* `max_out` bytes is
/// returned; one byte more is an error, and nothing is silently truncated.
///
/// # Arguments
/// * `data` - Compressed input data
/// * `compression` - Compression algorithm the data was compressed with
/// * `max_out` - Largest decompressed size accepted, in bytes
///
/// # Returns
/// Decompressed data, or a copy of the input if compression is None.
pub fn decompress(data: &[u8], compression: Compression, max_out: u64) -> io::Result<Vec<u8>> {
    dispatch(data, compression, max_out, DECOMPRESS)
}

/// Copy the input through, the compression side's `None` arm.
///
/// Infallible, but the `Result` is what the codec table's function pointers
/// are shaped like — the other three arms can all fail.
#[allow(clippy::unnecessary_wraps)]
fn copy_all(data: &[u8], _: ()) -> io::Result<Vec<u8>> {
    Ok(data.to_vec())
}

/// Copy the input through, the decompression side's `None` arm: the ceiling
/// applies to stored tiles too, so one code path decides how much a caller
/// may be handed.
fn copy_capped(data: &[u8], max_out: u64) -> io::Result<Vec<u8>> {
    if data.len() as u64 > max_out {
        return Err(too_big("stored", max_out));
    }
    Ok(data.to_vec())
}

/// Read a decompressor to its end, refusing to hold more than `max_out`
/// bytes.
///
/// `take(max_out + 1)` is what makes the check exact rather than truncating:
/// the extra byte is the evidence that the stream had more to give, and the
/// allocation stays bounded whatever the stream claims.
fn read_capped(reader: impl io::Read, max_out: u64, what: &'static str) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    reader
        .take(max_out.saturating_add(1))
        .read_to_end(&mut out)?;
    if out.len() as u64 > max_out {
        return Err(too_big(what, max_out));
    }
    Ok(out)
}

fn too_big(what: &str, max_out: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("{what} output exceeds the {max_out}-byte decompression ceiling"),
    )
}

/// Decompress gzip data, bounded by `max_out`.
fn decompress_gzip(data: &[u8], max_out: u64) -> io::Result<Vec<u8>> {
    read_capped(flate2::read::GzDecoder::new(data), max_out, "gzip")
}

/// Decompress brotli data, bounded by `max_out`.
fn decompress_brotli(data: &[u8], max_out: u64) -> io::Result<Vec<u8>> {
    read_capped(brotli::Decompressor::new(data, 4096), max_out, "brotli")
}

/// Decompress zstd data, bounded by `max_out`.
///
/// Streamed rather than `zstd::decode_all`, which sizes its buffer from the
/// frame's own content-size field and so cannot be bounded.
fn decompress_zstd(data: &[u8], max_out: u64) -> io::Result<Vec<u8>> {
    read_capped(zstd::stream::read::Decoder::new(data)?, max_out, "zstd")
}

/// Compress data with gzip.
fn compress_gzip(data: &[u8], _: ()) -> io::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression as GzCompression;

    let mut encoder = GzEncoder::new(Vec::new(), GzCompression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

/// Compress data with brotli.
fn compress_brotli(data: &[u8], _: ()) -> io::Result<Vec<u8>> {
    use brotli::enc::BrotliEncoderParams;
    use brotli::CompressorWriter;

    // Use quality level 4 - good balance of speed and compression
    // (tippecanoe uses quality 9, but we default to something faster)
    let params = BrotliEncoderParams {
        quality: 4,
        ..Default::default()
    };

    let mut output = Vec::new();
    {
        let mut writer = CompressorWriter::with_params(&mut output, 4096, &params);
        writer.write_all(data)?;
    }
    Ok(output)
}

/// Compress data with zstd.
fn compress_zstd(data: &[u8], _: ()) -> io::Result<Vec<u8>> {
    // Use compression level 3 (default) - good balance of speed and ratio
    zstd::encode_all(data, 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Compression Enum Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_compression_codes_match_pmtiles_spec() {
        // PMTiles v3 spec defines these exact byte values
        assert_eq!(Compression::Unknown.code(), 0);
        assert_eq!(Compression::None.code(), 1);
        assert_eq!(Compression::Gzip.code(), 2);
        assert_eq!(Compression::Brotli.code(), 3);
        assert_eq!(Compression::Zstd.code(), 4);
    }

    #[test]
    fn test_compression_default_is_gzip() {
        // Gzip is the default for maximum compatibility:
        // - Universally supported by all PMTiles viewers
        // - Works in pmtiles.io without issues
        // - Use --compression zstd for better performance when supported
        assert_eq!(Compression::default(), Compression::Gzip);
    }

    #[test]
    fn test_compression_from_str() {
        assert_eq!(Compression::from_str("none"), Some(Compression::None));
        assert_eq!(Compression::from_str("gzip"), Some(Compression::Gzip));
        assert_eq!(Compression::from_str("brotli"), Some(Compression::Brotli));
        assert_eq!(Compression::from_str("zstd"), Some(Compression::Zstd));
        assert_eq!(Compression::from_str("GZIP"), Some(Compression::Gzip)); // case insensitive
        assert_eq!(Compression::from_str("invalid"), Option::None);
    }

    #[test]
    fn test_compression_names() {
        assert_eq!(Compression::None.name(), "none");
        assert_eq!(Compression::Gzip.name(), "gzip");
        assert_eq!(Compression::Brotli.name(), "brotli");
        assert_eq!(Compression::Zstd.name(), "zstd");
    }

    // -------------------------------------------------------------------------
    // Compression Function Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_compress_none_returns_original() {
        let data = b"Hello, PMTiles!";
        let compressed = compress(data, Compression::None).unwrap();
        assert_eq!(compressed, data);
    }

    #[test]
    fn test_compress_unknown_returns_error() {
        let data = b"Hello, PMTiles!";
        let result = compress(data, Compression::Unknown);
        assert!(result.is_err());
    }

    #[test]
    fn test_compress_gzip_produces_smaller_output() {
        // Use a compressible pattern
        let data = "Hello, PMTiles! ".repeat(100);
        let compressed = compress(data.as_bytes(), Compression::Gzip).unwrap();
        assert!(
            compressed.len() < data.len(),
            "Gzip should compress repetitive data: {} < {}",
            compressed.len(),
            data.len()
        );
    }

    #[test]
    fn test_compress_brotli_produces_smaller_output() {
        let data = "Hello, PMTiles! ".repeat(100);
        let compressed = compress(data.as_bytes(), Compression::Brotli).unwrap();
        assert!(
            compressed.len() < data.len(),
            "Brotli should compress repetitive data: {} < {}",
            compressed.len(),
            data.len()
        );
    }

    #[test]
    fn test_compress_zstd_produces_smaller_output() {
        let data = "Hello, PMTiles! ".repeat(100);
        let compressed = compress(data.as_bytes(), Compression::Zstd).unwrap();
        assert!(
            compressed.len() < data.len(),
            "Zstd should compress repetitive data: {} < {}",
            compressed.len(),
            data.len()
        );
    }

    // -------------------------------------------------------------------------
    // Decompression Roundtrip Tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_gzip_roundtrip() {
        use flate2::read::GzDecoder;
        use std::io::Read;

        let original = b"Hello, PMTiles! This is test data for compression roundtrip.";
        let compressed = compress(original, Compression::Gzip).unwrap();

        let mut decoder = GzDecoder::new(&compressed[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_brotli_roundtrip() {
        use brotli::Decompressor;
        use std::io::Read;

        let original = b"Hello, PMTiles! This is test data for compression roundtrip.";
        let compressed = compress(original, Compression::Brotli).unwrap();

        let mut decompressor = Decompressor::new(&compressed[..], 4096);
        let mut decompressed = Vec::new();
        decompressor.read_to_end(&mut decompressed).unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_zstd_roundtrip() {
        let original = b"Hello, PMTiles! This is test data for compression roundtrip.";
        let compressed = compress(original, Compression::Zstd).unwrap();

        let decompressed = zstd::decode_all(&compressed[..]).unwrap();

        assert_eq!(decompressed, original);
    }

    // -------------------------------------------------------------------------
    // Edge Cases
    // -------------------------------------------------------------------------

    #[test]
    fn test_compress_empty_data() {
        // All compression types should handle empty input
        for compression in [
            Compression::None,
            Compression::Gzip,
            Compression::Brotli,
            Compression::Zstd,
        ] {
            let result = compress(&[], compression);
            assert!(
                result.is_ok(),
                "{} should handle empty data",
                compression.name()
            );
        }
    }

    #[test]
    fn test_compress_large_data() {
        // Simulate a large tile (~1MB of data)
        let data = vec![0x42u8; 1_000_000];

        for compression in [Compression::Gzip, Compression::Brotli, Compression::Zstd] {
            let result = compress(&data, compression);
            assert!(
                result.is_ok(),
                "{} should handle large data",
                compression.name()
            );

            let compressed = result.unwrap();
            // Highly repetitive data should compress very well
            assert!(
                compressed.len() < data.len() / 10,
                "{} should achieve >10x compression on uniform data",
                compression.name()
            );
        }
    }

    // -------------------------------------------------------------------------
    // Decompression (read side, issue #112)
    // -------------------------------------------------------------------------

    #[test]
    fn test_from_code_inverts_code() {
        for compression in [
            Compression::Unknown,
            Compression::None,
            Compression::Gzip,
            Compression::Brotli,
            Compression::Zstd,
        ] {
            assert_eq!(
                Compression::from_code(compression.code()),
                Some(compression)
            );
        }
        assert_eq!(Compression::from_code(5), None);
        assert_eq!(Compression::from_code(255), None);
    }

    #[test]
    fn test_decompress_roundtrips_every_codec() {
        let original = b"PMTiles round-trip payload \x00\x01\x02 with some repetition repetition";
        for compression in [
            Compression::None,
            Compression::Gzip,
            Compression::Brotli,
            Compression::Zstd,
        ] {
            let compressed = compress(original, compression).unwrap();
            let decompressed = decompress(&compressed, compression, MAX_INTERNAL_BYTES).unwrap();
            assert_eq!(
                decompressed,
                original.to_vec(),
                "{} round-trip",
                compression.name()
            );
        }
    }

    #[test]
    fn test_decompress_empty_payload() {
        for compression in [
            Compression::None,
            Compression::Gzip,
            Compression::Brotli,
            Compression::Zstd,
        ] {
            let compressed = compress(&[], compression).unwrap();
            let decompressed = decompress(&compressed, compression, MAX_INTERNAL_BYTES).unwrap();
            assert!(decompressed.is_empty(), "{}", compression.name());
        }
    }

    #[test]
    fn test_decompress_unknown_is_error() {
        assert!(decompress(b"anything", Compression::Unknown, MAX_INTERNAL_BYTES).is_err());
    }

    #[test]
    fn test_decompress_corrupt_gzip_is_error() {
        assert!(decompress(b"not gzip at all", Compression::Gzip, MAX_INTERNAL_BYTES).is_err());
    }

    // -------------------------------------------------------------------------
    // Decompression ceilings (#417)
    // -------------------------------------------------------------------------

    /// A megabyte of zeros: a few hundred bytes on the wire in every codec.
    const BOMB_PLAIN: usize = 1024 * 1024;

    #[test]
    fn decompress_refuses_output_past_max_out() {
        for compression in [
            Compression::None,
            Compression::Gzip,
            Compression::Brotli,
            Compression::Zstd,
        ] {
            let compressed = compress(&vec![0u8; BOMB_PLAIN], compression).unwrap();
            let err = decompress(&compressed, compression, 1024)
                .expect_err(compression.name())
                .to_string();
            assert!(
                err.contains("exceeds") && err.contains("1024"),
                "{}: {err}",
                compression.name()
            );
        }
    }

    #[test]
    fn decompress_ceiling_is_exact_not_truncating() {
        // Exactly at the ceiling is fine; one byte under must fail rather
        // than hand back a silently shortened buffer.
        for compression in [
            Compression::None,
            Compression::Gzip,
            Compression::Brotli,
            Compression::Zstd,
        ] {
            let plain = vec![7u8; BOMB_PLAIN];
            let compressed = compress(&plain, compression).unwrap();
            let exact = decompress(&compressed, compression, BOMB_PLAIN as u64).unwrap();
            assert_eq!(exact.len(), BOMB_PLAIN, "{}", compression.name());
            assert_eq!(exact, plain, "{}", compression.name());
            assert!(
                decompress(&compressed, compression, BOMB_PLAIN as u64 - 1).is_err(),
                "{} must not truncate to the ceiling",
                compression.name()
            );
        }
    }
}
