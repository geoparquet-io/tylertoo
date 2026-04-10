#![recursion_limit = "256"]
//! Core library for converting GeoParquet to PMTiles vector tiles.
//!
//! This library provides the foundational functionality for reading GeoParquet files
//! and converting them into PMTiles vector tile archives with MVT encoding.
//!
//! # Memory Profiling
//!
//! Build with `--features dhat-heap` to enable heap profiling with dhat.
//! When enabled, the program will write `dhat-heap.json` on exit which can
//! be analyzed at <https://nnethercote.github.io/dh_view/dh_view.html>
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

// dhat global allocator - must be at crate root
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::path::Path;
use thiserror::Error;

// Include the protobuf-generated code
pub mod vector_tile {
    include!(concat!(env!("OUT_DIR"), "/vector_tile.rs"));
}

pub mod accumulator;
pub mod adaptive;
pub mod batch_processor;
pub mod clip;
pub mod clustering;
pub mod coalesce;
pub mod compression;
pub mod covering;
pub mod dedup;
pub mod external_sort;
pub mod feature_drop;
pub mod gap_density;
#[cfg(test)]
mod golden;
pub mod hierarchical_clip;
#[cfg(test)]
mod integration_tests;
pub mod ioverlay_clip;
pub mod memory;
pub mod mvt;
pub mod pipeline;
pub mod pmtiles_writer;
pub mod property_filter;
pub mod quality;
pub mod sampling;
pub mod simplify;
pub mod spatial_index;
pub mod sutherland_hodgman;
pub mod tile;
pub mod validate;
pub mod wkb;
pub mod world_coord;

// Re-export accumulator types for CLI usage
pub use accumulator::{AccumulatorConfig, AccumulatorOp};
// Re-export clustering types for CLI usage
pub use clustering::{ClusterConfig, IndexedPoint, PointClusterer};
// Re-export coalescing types for CLI usage
pub use coalesce::{
    calculate_coalesce_targets, AttributeMode, CoalesceConfig, CoalesceTargets, GridSize,
};
// Re-export PropertyFilter for convenience
pub use property_filter::PropertyFilter;
// Re-export Compression from compression module for public API
pub use compression::Compression;
// Re-export StreamingPmtilesWriter and related types
pub use pmtiles_writer::{StreamingPmtilesWriter, StreamingWriteStats};
// Re-export progress types for CLI usage
pub use pipeline::{ProgressCallback, ProgressEvent};
// Re-export CRS validation for CLI usage
pub use quality::{extract_crs, validate_wgs84, CrsInfo};
// Re-export covering types for row group filtering
pub use covering::{
    extract_row_group_bounds, find_bbox_column_indices, parse_bounds, parse_covering_metadata,
    tile_to_bounds, BboxColumnIndices, CoveringSpec, RowGroupBounds,
};
// Re-export ProcessingMode for memory-bounded processing
pub use pipeline::{auto_bucket_count, auto_processing_mode, ProcessingMode};
// Re-export simplify functions for external use
pub use simplify::simplify_geometry_for_tile;

/// Format a helpful error message for CannotReduceFurther errors
fn format_cannot_reduce_error(
    zoom: u8,
    tile: &str,
    size: usize,
    features: usize,
    max_tile_size: usize,
) -> String {
    let bytes_per_feature = if features > 0 { size / features } else { size };
    let size_kb = size / 1024;
    let max_kb = max_tile_size / 1024;

    let mut msg = format!(
        "Cannot reduce tile {tile} (zoom {zoom}): {size_kb}KB with {features} features exceeds {max_kb}KB limit"
    );

    // Diagnose the problem and suggest solutions
    if bytes_per_feature > 50_000 {
        // Large average feature size = geometry complexity issue (regardless of count)
        msg.push_str(&format!(
            "\n\nDiagnosis: Each feature averages {}KB - geometries are too complex at this zoom level.",
            bytes_per_feature / 1024
        ));
        msg.push_str("\n\nSuggestions:");
        msg.push_str(&format!(
            "\n  • Increase --min-zoom to {} or higher to skip problematic low-zoom tiles",
            zoom + 2
        ));
        msg.push_str(&format!(
            "\n  • Increase --max-tile-size to {}M to allow larger tiles at low zoom",
            (size / 1_000_000) + 1
        ));
        msg.push_str("\n  • Pre-simplify geometries with `gpio simplify` before tiling");
    } else if features > max_tile_size / 100 {
        // Too many features
        msg.push_str("\n\nDiagnosis: Too many features for tile size limit.");
        msg.push_str("\n\nSuggestions:");
        msg.push_str("\n  • Increase --max-tile-features to allow more features");
        msg.push_str("\n  • Use --drop-densest-as-needed with higher --gamma (e.g., --gamma 3)");
        msg.push_str("\n  • Use --cluster-distance to merge nearby points");
    } else {
        msg.push_str("\n\nSuggestions:");
        msg.push_str(&format!(
            "\n  • Increase --max-tile-size (current: {}KB)",
            max_kb
        ));
        msg.push_str(&format!("\n  • Increase --min-zoom above {}", zoom));
    }

    msg
}

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

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{}", format_cannot_reduce_error(*.zoom, tile, *.size, *.features, *.max_tile_size))]
    CannotReduceFurther {
        tile: String,
        zoom: u8,
        size: usize,
        features: usize,
        max_tile_size: usize,
    },
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

        tracing::info!(
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

        tracing::info!("Generated {} tiles", tile_count);

        // Log deduplication stats
        let dedup_stats = writer.dedup_stats();
        if dedup_stats.duplicates_eliminated > 0 {
            tracing::info!(
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

        tracing::info!("Wrote PMTiles to {}", output_path.display());

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
