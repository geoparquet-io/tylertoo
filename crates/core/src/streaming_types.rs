//! Shared types for the streaming tile pipeline.
//!
//! This module defines the core types used across the streaming pipeline components:
//! - TileSpool
//! - StreamingTileBuffer
//! - PMTiles spool-based writer
//! - MLT/MVT encoding

use std::path::PathBuf;

/// Tile encoding format
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TileFormat {
    /// Mapbox Vector Tiles - maximum compatibility with all viewers
    #[default]
    Mvt,
    /// MapLibre Tiles - better compression (up to 6x on large tiles)
    /// Requires MLT-compatible viewer (MapLibre GL JS with MLT plugin)
    Mlt,
}

impl TileFormat {
    /// Returns the PMTiles tile type byte for this format
    pub fn pmtiles_tile_type(&self) -> u8 {
        match self {
            TileFormat::Mvt => 1, // MVT
            TileFormat::Mlt => 2, // MLT (assuming this is the correct value)
        }
    }

    /// Returns the MIME type for this format
    pub fn mime_type(&self) -> &'static str {
        match self {
            TileFormat::Mvt => "application/vnd.mapbox-vector-tile",
            TileFormat::Mlt => "application/vnd.maplibre-vector-tile",
        }
    }
}

impl std::str::FromStr for TileFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "mvt" => Ok(TileFormat::Mvt),
            "mlt" => Ok(TileFormat::Mlt),
            _ => Err(format!(
                "Unknown tile format: {}. Expected 'mvt' or 'mlt'",
                s
            )),
        }
    }
}

impl std::fmt::Display for TileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TileFormat::Mvt => write!(f, "mvt"),
            TileFormat::Mlt => write!(f, "mlt"),
        }
    }
}

/// Entry in the tile spool index.
///
/// The spool stores encoded tiles in arrival order (not tile_id order).
/// Multiple entries for the same tile_id are allowed (sparse spool pattern
/// for handling late arrivals). Only the last entry per tile_id is used
/// when building the final PMTiles file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpoolEntry {
    /// PMTiles tile_id (z/x/y encoded as u64)
    pub tile_id: u64,
    /// Offset within the spool file (NOT the final PMTiles offset)
    pub spool_offset: u64,
    /// Length of the encoded tile data in bytes
    pub length: u32,
}

/// Sorting strategy for tile generation
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortingStrategy {
    /// Let the system decide based on observed metrics.
    /// Starts with streaming, falls back to external sort if input is poorly sorted.
    #[default]
    Auto,
    /// Streaming with spool - optimal for Hilbert-sorted input (via `gpio optimize`).
    /// Uses minimal temp disk (~1x output size) and memory (~100-500MB).
    Streaming,
    /// External sort - works with any input but uses more resources.
    /// Uses ~2x input size temp disk and ~2GB memory.
    ExternalSort,
}

impl std::str::FromStr for SortingStrategy {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(SortingStrategy::Auto),
            "streaming" => Ok(SortingStrategy::Streaming),
            "external" | "external-sort" => Ok(SortingStrategy::ExternalSort),
            _ => Err(format!(
                "Unknown sorting strategy: {}. Expected 'auto', 'streaming', or 'external'",
                s
            )),
        }
    }
}

impl std::fmt::Display for SortingStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SortingStrategy::Auto => write!(f, "auto"),
            SortingStrategy::Streaming => write!(f, "streaming"),
            SortingStrategy::ExternalSort => write!(f, "external"),
        }
    }
}

/// Configuration for the streaming tile buffer
#[derive(Clone, Debug)]
pub struct StreamingConfig {
    /// Maximum number of active tiles to keep in memory before forcing eviction
    pub max_active_tiles: usize,
    /// Tile encoding format (MVT or MLT)
    pub tile_format: TileFormat,
    /// Warn if late arrival rate exceeds this threshold (0.0-1.0)
    pub late_arrival_warn_threshold: f64,
    /// Fallback to external sort if late arrival rate exceeds this (0.0-1.0)
    pub late_arrival_fallback_threshold: f64,
    /// Number of tiles to batch before parallel encoding
    pub parallel_batch_size: usize,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            max_active_tiles: 500,
            tile_format: TileFormat::default(),
            late_arrival_warn_threshold: 0.05,     // 5%
            late_arrival_fallback_threshold: 0.20, // 20%
            parallel_batch_size: 16,
        }
    }
}

/// Statistics from streaming tile generation
#[derive(Clone, Debug, Default)]
pub struct StreamingStats {
    /// Number of tiles flushed to spool
    pub tiles_flushed: u64,
    /// Number of features processed
    pub features_processed: u64,
    /// Number of features arriving for already-flushed tiles
    pub late_arrivals: u64,
    /// Number of tiles evicted due to memory pressure
    pub evictions: u64,
}

impl StreamingStats {
    /// Calculate the late arrival rate (0.0-1.0)
    pub fn late_arrival_rate(&self) -> f64 {
        if self.features_processed == 0 {
            0.0
        } else {
            self.late_arrivals as f64 / self.features_processed as f64
        }
    }

    /// Calculate the eviction rate (0.0-1.0)
    pub fn eviction_rate(&self) -> f64 {
        if self.tiles_flushed == 0 {
            0.0
        } else {
            self.evictions as f64 / self.tiles_flushed as f64
        }
    }
}

/// Reason for recommending fallback to external sort
#[derive(Clone, Debug)]
pub enum FallbackReason {
    /// Late arrival rate exceeded threshold
    HighLateArrivalRate(f64),
    /// Eviction rate exceeded threshold (memory pressure)
    HighEvictionRate(f64),
}

impl FallbackReason {
    /// Get a user-friendly message explaining the fallback reason
    pub fn message(&self) -> String {
        match self {
            Self::HighLateArrivalRate(rate) => format!(
                "Input appears unsorted ({:.1}% late arrivals). \
                Consider: (1) run `gpio optimize input.parquet` first, or \
                (2) use `--sorting-strategy external` to force external sort.",
                rate * 100.0
            ),
            Self::HighEvictionRate(rate) => format!(
                "High memory pressure ({:.1}% eviction rate). \
                Consider: (1) increase --max-active-tiles, or \
                (2) use `--sorting-strategy external` for unsorted input.",
                rate * 100.0
            ),
        }
    }
}

/// Result of spool finalization - the path and deduplicated entries
#[derive(Debug)]
pub struct SpoolResult {
    /// Path to the spool temp file
    pub path: PathBuf,
    /// Deduplicated and sorted entries (ready for PMTiles writing)
    pub entries: Vec<SpoolEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tile_format_default() {
        assert_eq!(TileFormat::default(), TileFormat::Mvt);
    }

    #[test]
    fn test_tile_format_from_str() {
        assert_eq!("mvt".parse::<TileFormat>().unwrap(), TileFormat::Mvt);
        assert_eq!("MVT".parse::<TileFormat>().unwrap(), TileFormat::Mvt);
        assert_eq!("mlt".parse::<TileFormat>().unwrap(), TileFormat::Mlt);
        assert_eq!("MLT".parse::<TileFormat>().unwrap(), TileFormat::Mlt);
        assert!("unknown".parse::<TileFormat>().is_err());
    }

    #[test]
    fn test_sorting_strategy_from_str() {
        assert_eq!(
            "auto".parse::<SortingStrategy>().unwrap(),
            SortingStrategy::Auto
        );
        assert_eq!(
            "streaming".parse::<SortingStrategy>().unwrap(),
            SortingStrategy::Streaming
        );
        assert_eq!(
            "external".parse::<SortingStrategy>().unwrap(),
            SortingStrategy::ExternalSort
        );
        assert!("unknown".parse::<SortingStrategy>().is_err());
    }

    #[test]
    fn test_streaming_stats_rates() {
        let stats = StreamingStats {
            tiles_flushed: 100,
            features_processed: 1000,
            late_arrivals: 50,
            evictions: 10,
        };
        assert!((stats.late_arrival_rate() - 0.05).abs() < 0.001);
        assert!((stats.eviction_rate() - 0.1).abs() < 0.001);
    }

    #[test]
    fn test_streaming_stats_zero_division() {
        let stats = StreamingStats::default();
        assert_eq!(stats.late_arrival_rate(), 0.0);
        assert_eq!(stats.eviction_rate(), 0.0);
    }

    #[test]
    fn test_streaming_config_default() {
        let config = StreamingConfig::default();
        assert_eq!(config.max_active_tiles, 500);
        assert_eq!(config.tile_format, TileFormat::Mvt);
        assert!((config.late_arrival_warn_threshold - 0.05).abs() < 0.001);
        assert!((config.late_arrival_fallback_threshold - 0.20).abs() < 0.001);
        assert_eq!(config.parallel_batch_size, 16);
    }
}
