#![recursion_limit = "256"]
//! Core library for converting GeoParquet to PMTiles vector tiles.
//!
//! This library provides the foundational functionality for reading GeoParquet files
//! and converting them into PMTiles vector tile archives with MVT encoding.
//!
//! # Examples
//!
//! ```no_run
//! use gpq_tiles_core::{Converter, Config};
//!
//! let config = Config {
//!     min_zoom: 0,
//!     max_zoom: 14,
//!     ..Default::default()
//! };
//!
//! let converter = Converter::new(config);
//! converter.convert("input.parquet", "output.pmtiles").unwrap();
//! ```

use std::path::Path;
use thiserror::Error;

// Include the protobuf-generated code
pub mod vector_tile {
    include!(concat!(env!("OUT_DIR"), "/vector_tile.rs"));
}

pub mod batch_processor;
pub mod clip;
pub mod compression;
pub mod dedup;
pub mod external_sort;
pub mod feature_drop;
#[cfg(test)]
mod golden;
pub mod hierarchical_clip;
#[cfg(test)]
mod integration_tests;
pub mod memory;
pub mod mvt;
pub mod pipeline;
pub mod pmtiles_writer;
pub mod property_filter;
pub mod quality;
pub mod simplify;
pub mod spatial_index;
pub mod sutherland_hodgman;
pub mod tile;
pub mod validate;
pub mod wagyu_clip;
pub mod wkb;
pub mod world_coord;

// Re-export PropertyFilter for convenience
pub use property_filter::PropertyFilter;
// Re-export Compression from compression module for public API
pub use compression::Compression;
// Re-export StreamingPmtilesWriter and related types
pub use pmtiles_writer::{StreamingPmtilesWriter, StreamingWriteStats};
// Re-export progress types for CLI usage
pub use pipeline::{ProgressCallback, ProgressEvent};

/// Errors that can occur during GeoParquet to PMTiles conversion
#[derive(Error, Debug)]
pub enum Error {
    #[error("Failed to read GeoParquet file: {0}")]
    GeoParquetRead(String),

    #[error("Failed to write PMTiles: {0}")]
    PMTilesWrite(String),

    #[error("Invalid geometry at feature {feature_id}: {reason}")]
    InvalidGeometry { feature_id: usize, reason: String },

    #[error("MVT encoding failed: {0}")]
    MvtEncoding(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Configuration for the GeoParquet to PMTiles conversion
#[derive(Debug, Clone)]
pub struct Config {
    /// Minimum zoom level to generate
    pub min_zoom: u8,
    /// Maximum zoom level to generate
    pub max_zoom: u8,
    /// Tile extent (default: 4096 as per MVT spec)
    pub extent: u32,
    /// Feature dropping density threshold
    pub drop_density: DropDensity,
    /// Layer name for the MVT output (None = derive from input filename)
    pub layer_name: Option<String>,
    /// Property filter for controlling which attributes are included in output tiles.
    /// Matches tippecanoe's -x (exclude), -y (include), and -X (exclude-all) flags.
    pub property_filter: property_filter::PropertyFilter,
    /// Compression algorithm for tile data (default: Gzip)
    pub compression: Compression,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            min_zoom: 0,
            max_zoom: 14,
            extent: 4096,
            drop_density: DropDensity::Medium,
            layer_name: None,
            property_filter: property_filter::PropertyFilter::None,
            compression: Compression::default(),
        }
    }
}

/// Feature dropping density levels
#[derive(Debug, Clone, Copy)]
pub enum DropDensity {
    Low,
    Medium,
    High,
}

/// Main converter struct
pub struct Converter {
    #[allow(dead_code)] // Used in future phases
    config: Config,
}

impl Converter {
    /// Create a new converter with the given configuration
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Convert a GeoParquet file to PMTiles
    ///
    /// Generates vector tiles from the input GeoParquet file and writes them
    /// to a PMTiles archive. Uses the configuration provided at construction.
    pub fn convert<P: AsRef<Path>, Q: AsRef<Path>>(&self, input: P, output: Q) -> Result<()> {
        use crate::pipeline::{generate_tiles_with_bounds, TilerConfig};
        use crate::pmtiles_writer::PmtilesWriter;

        let input_path = input.as_ref();
        let output_path = output.as_ref();

        log::info!(
            "Converting {} to {}",
            input_path.display(),
            output_path.display()
        );

        // Derive layer name from input filename if not specified
        let layer_name = self.config.layer_name.clone().unwrap_or_else(|| {
            input_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("layer")
                .to_string()
        });

        // Build TilerConfig from our Config
        let tiler_config = TilerConfig::new(self.config.min_zoom, self.config.max_zoom)
            .with_extent(self.config.extent)
            .with_layer_name(&layer_name)
            .with_density_drop(matches!(
                self.config.drop_density,
                DropDensity::Medium | DropDensity::High
            ))
            .with_density_max_per_cell(match self.config.drop_density {
                DropDensity::Low => 10,
                DropDensity::Medium => 3,
                DropDensity::High => 1,
            })
            .with_property_filter(self.config.property_filter.clone());

        // Generate tiles using the pipeline (with bounds for PMTiles header)
        let tile_gen = generate_tiles_with_bounds(input_path, &tiler_config)
            .map_err(|e| Error::GeoParquetRead(e.to_string()))?;

        // Write tiles to PMTiles with proper bounds, layer name, field metadata, compression, and deduplication
        let mut writer = PmtilesWriter::with_compression(self.config.compression);
        writer.enable_deduplication(true); // Enable deduplication by default
        writer.set_bounds(&tile_gen.bounds);
        writer.set_layer_name(&tile_gen.layer_name);
        writer.set_fields(tile_gen.fields);

        let mut tile_count = 0;
        for tile_result in tile_gen.tiles {
            let tile = tile_result.map_err(|e| Error::MvtEncoding(e.to_string()))?;
            writer
                .add_tile_with_count(
                    tile.coord.z,
                    tile.coord.x,
                    tile.coord.y,
                    &tile.data,
                    tile.feature_count,
                )
                .map_err(|e| Error::PMTilesWrite(e.to_string()))?;
            tile_count += 1;
        }

        log::info!("Generated {} tiles", tile_count);

        // Log deduplication stats
        let dedup_stats = writer.dedup_stats();
        if dedup_stats.duplicates_eliminated > 0 {
            log::info!(
                "Deduplication: {} unique tiles, {} duplicates eliminated ({:.1}% savings)",
                dedup_stats.unique_tiles,
                dedup_stats.duplicates_eliminated,
                dedup_stats.savings_percent()
            );
        }

        // Write to file
        writer
            .write_to_file(output_path)
            .map_err(|e| Error::PMTilesWrite(e.to_string()))?;

        log::info!("Wrote PMTiles to {}", output_path.display());

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.min_zoom, 0);
        assert_eq!(config.max_zoom, 14);
        assert_eq!(config.extent, 4096);
    }

    #[test]
    fn test_converter_creation() {
        let config = Config::default();
        let _converter = Converter::new(config);
        // If we get here without panicking, the test passes
    }

    #[test]
    fn test_convert_nonexistent_file() {
        let config = Config::default();
        let converter = Converter::new(config);

        let result = converter.convert("/nonexistent/file.parquet", "/tmp/output.pmtiles");

        assert!(result.is_err());
        match result {
            Err(Error::GeoParquetRead(_)) => {} // Expected error type
            _ => panic!("Expected GeoParquetRead error"),
        }
    }

    #[test]
    fn test_convert_with_real_fixture() {
        let config = Config::default();
        let converter = Converter::new(config);

        // Use one of our real fixtures
        let fixture = "../../tests/fixtures/realdata/open-buildings.parquet";
        let output = "/tmp/test-output.pmtiles";

        // Only run if fixture exists
        if Path::new(fixture).exists() {
            let result = converter.convert(fixture, output);
            assert!(result.is_ok());

            // Verify output was created
            assert!(Path::new(output).exists());

            // Clean up
            let _ = std::fs::remove_file(output);
        }
    }
}
