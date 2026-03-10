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

/// Compress data using the specified algorithm.
///
/// # Arguments
/// * `data` - Uncompressed input data
/// * `compression` - Compression algorithm to use
///
/// # Returns
/// Compressed data, or original data if compression is None.
pub fn compress(data: &[u8], compression: Compression) -> io::Result<Vec<u8>> {
    match compression {
        Compression::Unknown => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Cannot compress with unknown compression type",
        )),
        Compression::None => Ok(data.to_vec()),
        Compression::Gzip => compress_gzip(data),
        Compression::Brotli => compress_brotli(data),
        Compression::Zstd => compress_zstd(data),
    }
}

/// Compress data with gzip.
fn compress_gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression as GzCompression;

    let mut encoder = GzEncoder::new(Vec::new(), GzCompression::default());
    encoder.write_all(data)?;
    encoder.finish()
}

/// Compress data with brotli.
fn compress_brotli(data: &[u8]) -> io::Result<Vec<u8>> {
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
fn compress_zstd(data: &[u8]) -> io::Result<Vec<u8>> {
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
}
