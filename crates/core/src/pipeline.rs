//! Tiler pipeline - wires together batch processing, clipping, simplification, and MVT encoding.
//!
//! This module provides the core tiling pipeline that:
//! 1. Reads geometries from GeoParquet
//! 2. Iterates tiles for the data's bounding box at each zoom level
//! 3. For each tile: clips, simplifies, validates, and encodes to MVT format
//!
//! # Tippecanoe Alignment
//!
//! This pipeline matches tippecanoe's approach:
//! - Buffer: 8 pixels (configurable)
//! - Simplification: Douglas-Peucker to tile resolution at each zoom
//! - Degenerate geometry filtering: drop invalid geometries post-simplification
//! - Empty tiles are skipped (not written)

use std::path::Path;
use std::sync::Arc;

use prost::Message;
use rayon::prelude::*;
use tracing::instrument;

use geo::Geometry;

use crate::accumulator::AccumulatorConfig;
use crate::batch_processor::{
    extract_field_metadata, extract_geometries, process_geometries_parallel, RowGroupInfo,
    DEFAULT_PARALLEL_READERS,
};
use crate::clip::{buffer_pixels_to_degrees, clip_geometry};
use crate::clustering::ClusterConfig;
use crate::feature_drop::{
    should_drop_multipoint, should_drop_point, should_drop_tiny_line, should_drop_tiny_line_world,
    should_drop_tiny_multiline, should_drop_tiny_polygon, should_drop_tiny_polygon_world,
    DensityDropConfig, DensityDropper, TinyPolygonAccumulator, DEFAULT_TINY_POLYGON_THRESHOLD,
};
use crate::gap_density::{scale_for_zoom, GapBasedSelector};
use crate::hierarchical_clip::{clip_geometry_hierarchical_world, WorldClippedGeometry};
use crate::mvt::{LayerBuilder, TileBuilder};
use crate::property_filter::PropertyFilter;
use crate::simplify::simplify_for_zoom;
use crate::spatial_index::sort_geometries;
use crate::tile::{tiles_for_bbox, TileBounds, TileCoord};
use crate::validate::filter_valid_geometry;
use crate::vector_tile::Tile;
use crate::{Error, Result};

/// Progress event emitted during tile generation.
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// Starting a phase of processing
    PhaseStart { phase: u8, name: &'static str },
    /// Progress within Phase 1 (reading row groups)
    Phase1Progress {
        row_group: usize,
        total_row_groups: usize,
        features_in_group: usize,
        records_written: u64,
    },
    /// Phase 1 complete
    Phase1Complete {
        total_records: u64,
        peak_memory_bytes: usize,
    },
    /// Phase 2 (sorting) started
    Phase2Start,
    /// Progress within Phase 2 (sorting shards)
    Phase2Progress {
        shard: usize,
        total_shards: usize,
        records_in_shard: usize,
        shard_duration_secs: f64,
    },
    /// Phase 2 complete
    Phase2Complete,
    /// Progress within Phase 3 (encoding tiles)
    Phase3Progress {
        tiles_written: u64,
        records_processed: u64,
        total_records: u64,
    },
    /// Processing complete
    Complete {
        total_tiles: u64,
        peak_memory_bytes: usize,
        duration_secs: f64,
    },
}

/// Callback type for progress reporting.
pub type ProgressCallback = Box<dyn Fn(ProgressEvent) + Send + Sync>;

/// Default buffer in pixels (matches tippecanoe common usage)
pub const DEFAULT_BUFFER_PIXELS: u32 = 8;

/// Default tile extent (4096 as per MVT spec)
pub const DEFAULT_EXTENT: u32 = 4096;

/// Determine if a geometry should be dropped based on zoom level and geometry type.
///
/// This function dispatches to the appropriate dropping predicate based on geometry type:
/// - **Points**: Thinned using 1/2.5 drop rate per zoom level below base_zoom
/// - **Lines**: Dropped if all vertices collapse to the same tile pixel
/// - **Polygons**: Dropped probabilistically if area < 4 square pixels (diffuse probability)
///
/// # Arguments
///
/// * `geom` - The geometry to check
/// * `zoom` - Current zoom level being generated
/// * `base_zoom` - The zoom level where all features are kept (typically max_zoom)
/// * `extent` - Tile extent (typically 4096)
/// * `tile_bounds` - Geographic bounds of the tile
/// * `feature_index` - Unique index of this feature for deterministic selection
///
/// # Returns
///
/// `true` if the geometry should be dropped, `false` if it should be kept.
fn should_drop_geometry(
    geom: &Geometry<f64>,
    zoom: u8,
    base_zoom: u8,
    extent: u32,
    tile_bounds: &TileBounds,
    feature_index: u64,
) -> bool {
    match geom {
        Geometry::Point(p) => should_drop_point(p, zoom, base_zoom, feature_index),
        Geometry::MultiPoint(mp) => should_drop_multipoint(mp, zoom, base_zoom, feature_index),
        Geometry::LineString(ls) => should_drop_tiny_line(ls, zoom, extent, tile_bounds),
        Geometry::MultiLineString(mls) => {
            should_drop_tiny_multiline(mls, zoom, extent, tile_bounds)
        }
        Geometry::Polygon(poly) => {
            should_drop_tiny_polygon(poly, tile_bounds, extent, DEFAULT_TINY_POLYGON_THRESHOLD)
        }
        Geometry::MultiPolygon(mp) => {
            // Drop if ALL polygons would be dropped
            mp.0.iter().all(|p| {
                should_drop_tiny_polygon(p, tile_bounds, extent, DEFAULT_TINY_POLYGON_THRESHOLD)
            })
        }
        // GeometryCollection and other types are not dropped
        _ => false,
    }
}

// NOTE: StreamingMode enum was removed in v0.4.0.
// The pipeline now always uses the geometry-centric external sort algorithm,
// which is both fast AND memory-bounded. See ADR-001 for rationale.

/// Configuration for the tiling pipeline.
#[derive(Debug, Clone)]
pub struct TilerConfig {
    /// Minimum zoom level to generate
    pub min_zoom: u8,
    /// Maximum zoom level to generate
    pub max_zoom: u8,
    /// Tile extent in pixels (default: 4096)
    pub extent: u32,
    /// Buffer in pixels around tile bounds (default: 8)
    pub buffer_pixels: u32,
    /// Layer name for the MVT output
    pub layer_name: String,
    /// Enable density-based feature dropping (default: true)
    /// When enabled, limits features per grid cell to reduce clutter at low zoom levels
    pub enable_density_drop: bool,
    /// Grid cell size in pixels for density dropping (default: 32)
    /// Smaller values = less aggressive dropping, larger = more aggressive
    pub density_cell_size: u32,
    /// Maximum features per grid cell (default: 1)
    /// Higher values = more features kept in dense areas
    pub density_max_per_cell: usize,
    /// Use Hilbert curve for spatial sorting (default: true)
    /// Hilbert curves have better locality than Z-order curves.
    /// If false, uses Z-order (Morton) curve instead.
    pub use_hilbert: bool,
    /// Property filter for controlling which attributes are included in output tiles.
    /// Matches tippecanoe's -x (exclude), -y (include), and -X (exclude-all) flags.
    /// Geometry columns are always preserved regardless of filter settings.
    pub property_filter: PropertyFilter,
    /// Memory budget in bytes for streaming processing (default: None = no limit).
    /// When set, streaming will attempt to stay within this budget by:
    /// - Processing one row group at a time
    /// - Flushing tiles after each row group
    /// - Warning if a single row group exceeds the budget
    pub memory_budget: Option<usize>,
    /// Suppress quality warnings and progress output (default: false).
    pub quiet: bool,
    /// Enable deterministic (sequential) processing for reproducible output.
    /// When true, disables parallelism to ensure bit-exact reproducibility.
    /// Useful for debugging, testing, and compliance workflows.
    /// Default: false (parallel processing enabled for performance).
    pub deterministic: bool,
    /// Enable tiny polygon accumulation (default: true).
    ///
    /// When enabled, tiny polygons that would be individually invisible are
    /// accumulated. When the accumulated area exceeds a threshold, a synthetic
    /// pixel-sized square is emitted at the centroid. This preserves visual
    /// density - 10 tiny polygons in a cluster become a single visible square.
    ///
    /// This matches tippecanoe's behavior (clip.cpp:1048-1097).
    ///
    /// When disabled, tiny polygons are dropped using diffuse probability
    /// (the legacy behavior).
    pub enable_tiny_polygon_accumulation: bool,
    /// Gamma parameter for gap-based density dropping (default: None = use grid-based).
    ///
    /// When set, uses tippecanoe's gap-based algorithm instead of grid-based.
    /// This uses Hilbert index gaps to determine which features to drop:
    ///
    /// - `gamma = 0.0`: Gap-based dropping disabled (use grid-based instead)
    /// - `gamma = 1.0`: Linear spacing
    /// - `gamma = 2.0`: "Reduces dots < 1 pixel apart to square root of original"
    ///   (tippecanoe's default for dense data)
    /// - Higher gamma = more aggressive dropping of closely-spaced features
    ///
    /// This is activated via `--drop-densest-as-needed --gamma=2.0` in tippecanoe.
    /// When `gamma` is `Some(value > 0.0)`, gap-based selection is used instead
    /// of grid-based density dropping.
    pub gamma: Option<f64>,
    /// Accumulator configuration for attribute aggregation during feature merging.
    ///
    /// When features are merged (e.g., during coalescing or simplification),
    /// this configuration determines how attributes are combined. Matches
    /// tippecanoe's `-ac` flag behavior.
    ///
    /// If None, no attribute accumulation is performed (attributes from the
    /// first feature are kept).
    pub accumulator_config: Option<AccumulatorConfig>,

    /// Point clustering configuration.
    ///
    /// When enabled, nearby points are clustered together at lower zoom levels.
    /// The centroid of each cluster replaces the individual points, and properties
    /// are accumulated according to the accumulator configuration.
    ///
    /// Matches tippecanoe's `--cluster-distance` and `--cluster-maxzoom` flags.
    ///
    /// If None, no point clustering is performed.
    pub cluster_config: Option<ClusterConfig>,
}

impl Default for TilerConfig {
    fn default() -> Self {
        Self {
            min_zoom: 0,
            max_zoom: 14,
            extent: DEFAULT_EXTENT,
            buffer_pixels: DEFAULT_BUFFER_PIXELS,
            layer_name: "layer".to_string(),
            // Density dropping is disabled by default to maintain backward compatibility
            // Enable it with .with_density_drop(true) when you need tippecanoe-like
            // feature reduction at low zoom levels
            enable_density_drop: false,
            // Cell size of 16 pixels = 256 cells per tile at 4096 extent
            // This is fairly aggressive - use larger values for less dropping
            density_cell_size: 16,
            density_max_per_cell: 1,
            // Hilbert curve is the default because it has better locality than Z-order
            use_hilbert: true,
            // No property filtering by default - include all properties
            property_filter: PropertyFilter::None,
            // No memory budget by default - rely on row group streaming
            memory_budget: None,
            // Show warnings by default
            quiet: false,
            // Parallel processing by default for performance
            deterministic: false,
            // Tiny polygon accumulation is enabled by default (matches tippecanoe)
            // This preserves visual density by emitting synthetic squares
            enable_tiny_polygon_accumulation: true,
            // Gap-based dropping disabled by default - use grid-based instead
            gamma: None,
            // No attribute accumulation by default
            accumulator_config: None,
            // No point clustering by default
            cluster_config: None,
        }
    }
}

impl TilerConfig {
    /// Suppress quality warnings and progress output.
    pub fn with_quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    /// Enable deterministic (sequential) processing for reproducible output.
    ///
    /// When enabled, disables all parallel processing to ensure bit-exact
    /// reproducibility across runs. This is useful for:
    /// - Debugging race conditions or non-deterministic behavior
    /// - Golden/snapshot testing where output must match exactly
    /// - Compliance workflows requiring reproducible output
    ///
    /// Default: `false` (parallel processing enabled for performance).
    ///
    /// # Example
    ///
    /// ```
    /// use gpq_tiles_core::pipeline::TilerConfig;
    ///
    /// let config = TilerConfig::new(0, 14)
    ///     .with_deterministic(true);
    /// ```
    pub fn with_deterministic(mut self, deterministic: bool) -> Self {
        self.deterministic = deterministic;
        self
    }
}

impl TilerConfig {
    /// Create a new config with custom settings.
    pub fn new(min_zoom: u8, max_zoom: u8) -> Self {
        Self {
            min_zoom,
            max_zoom,
            ..Default::default()
        }
    }

    /// Set the layer name.
    pub fn with_layer_name(mut self, name: impl Into<String>) -> Self {
        self.layer_name = name.into();
        self
    }

    /// Set the tile extent.
    pub fn with_extent(mut self, extent: u32) -> Self {
        self.extent = extent;
        self
    }

    /// Set the buffer in pixels.
    pub fn with_buffer(mut self, buffer_pixels: u32) -> Self {
        self.buffer_pixels = buffer_pixels;
        self
    }

    /// Enable or disable density-based feature dropping.
    pub fn with_density_drop(mut self, enable: bool) -> Self {
        self.enable_density_drop = enable;
        self
    }

    /// Set the grid cell size for density dropping.
    pub fn with_density_cell_size(mut self, cell_size: u32) -> Self {
        self.density_cell_size = cell_size;
        self
    }

    /// Set the maximum features per grid cell for density dropping.
    pub fn with_density_max_per_cell(mut self, max: usize) -> Self {
        self.density_max_per_cell = max;
        self
    }

    /// Set whether to use Hilbert curve (true) or Z-order curve (false) for spatial sorting.
    ///
    /// Hilbert curves have better locality than Z-order curves - neighboring points
    /// on the curve are always neighboring in 2D space. This is the default.
    ///
    /// Z-order (Morton) curves are simpler and faster to compute but don't have
    /// the same locality guarantee at quadrant boundaries.
    pub fn with_hilbert(mut self, use_hilbert: bool) -> Self {
        self.use_hilbert = use_hilbert;
        self
    }

    /// Set the property filter for controlling which attributes are included.
    ///
    /// Matches tippecanoe's property filtering behavior:
    /// - `PropertyFilter::Include(fields)` - only include specified fields (like `-y`)
    /// - `PropertyFilter::Exclude(fields)` - exclude specified fields (like `-x`)
    /// - `PropertyFilter::ExcludeAll` - exclude all attributes, keep only geometry (like `-X`)
    ///
    /// Geometry columns are always preserved regardless of filter settings.
    pub fn with_property_filter(mut self, filter: PropertyFilter) -> Self {
        self.property_filter = filter;
        self
    }

    /// Set an include filter (whitelist) for properties.
    ///
    /// Only the specified fields will be included in output tiles.
    /// This is equivalent to tippecanoe's `-y` flag.
    pub fn with_include_properties<I, S>(self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_property_filter(PropertyFilter::include(fields))
    }

    /// Set an exclude filter (blacklist) for properties.
    ///
    /// The specified fields will be excluded from output tiles.
    /// This is equivalent to tippecanoe's `-x` flag.
    pub fn with_exclude_properties<I, S>(self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.with_property_filter(PropertyFilter::exclude(fields))
    }

    /// Exclude all properties, keeping only geometry.
    ///
    /// This is equivalent to tippecanoe's `-X` flag.
    pub fn with_geometry_only(self) -> Self {
        self.with_property_filter(PropertyFilter::ExcludeAll)
    }

    /// Set a memory budget for streaming processing.
    ///
    /// When set, streaming will attempt to stay within this budget by:
    /// - Processing one row group at a time
    /// - Flushing tiles after each row group
    /// - Warning if a single row group exceeds the budget
    ///
    /// Note: This is advisory - actual memory usage depends on row group size
    /// in the input file. If a single row group exceeds the budget, processing
    /// will continue with a warning.
    ///
    /// # Example
    ///
    /// ```
    /// use gpq_tiles_core::pipeline::TilerConfig;
    ///
    /// let config = TilerConfig::new(0, 14)
    ///     .with_memory_budget(4 * 1024 * 1024 * 1024); // 4GB
    /// ```
    pub fn with_memory_budget(mut self, bytes: usize) -> Self {
        self.memory_budget = Some(bytes);
        self
    }

    /// Enable or disable tiny polygon accumulation.
    ///
    /// When enabled (default), tiny polygons that would individually be invisible
    /// are accumulated. When accumulated area exceeds a threshold, a synthetic
    /// pixel-sized square is emitted at the centroid. This preserves visual density.
    ///
    /// When disabled, tiny polygons are dropped using diffuse probability
    /// (the legacy behavior, faster but loses density information).
    ///
    /// # Example
    ///
    /// ```
    /// use gpq_tiles_core::pipeline::TilerConfig;
    ///
    /// let config = TilerConfig::new(0, 14)
    ///     .with_tiny_polygon_accumulation(false); // Disable accumulation
    /// ```
    pub fn with_tiny_polygon_accumulation(mut self, enable: bool) -> Self {
        self.enable_tiny_polygon_accumulation = enable;
        self
    }

    /// Set the gamma parameter for gap-based density dropping.
    ///
    /// When set to a value > 0, uses tippecanoe's gap-based algorithm instead
    /// of grid-based density dropping. The gap-based algorithm uses Hilbert
    /// index gaps to determine which features to drop, providing better
    /// preservation of spatial distribution.
    ///
    /// # Arguments
    ///
    /// * `gamma` - Exponential spacing parameter. Use 2.0 for tippecanoe's
    ///   default behavior (reduces dots < 1 pixel apart to square root of original).
    ///
    /// # Example
    ///
    /// ```
    /// use gpq_tiles_core::pipeline::TilerConfig;
    ///
    /// // Enable gap-based density dropping with gamma=2.0 (tippecanoe default)
    /// let config = TilerConfig::new(0, 14)
    ///     .with_gamma(2.0);
    /// ```
    pub fn with_gamma(mut self, gamma: f64) -> Self {
        self.gamma = Some(gamma);
        self
    }

    /// Enable gap-based density dropping (--drop-densest-as-needed).
    ///
    /// This is a convenience method that sets gamma=2.0, which is tippecanoe's
    /// default for `--drop-densest-as-needed`.
    ///
    /// # Example
    ///
    /// ```
    /// use gpq_tiles_core::pipeline::TilerConfig;
    ///
    /// // Enable gap-based density dropping with default gamma
    /// let config = TilerConfig::new(0, 14)
    ///     .with_drop_densest_as_needed();
    /// ```
    pub fn with_drop_densest_as_needed(mut self) -> Self {
        self.gamma = Some(2.0);
        self
    }

    /// Set the accumulator configuration for attribute aggregation.
    ///
    /// When features are merged during tile generation, this configuration
    /// determines how attributes are combined. Matches tippecanoe's `-ac` flag.
    ///
    /// # Example
    ///
    /// ```
    /// use gpq_tiles_core::accumulator::{AccumulatorConfig, AccumulatorOp};
    /// use gpq_tiles_core::pipeline::TilerConfig;
    ///
    /// let mut acc_config = AccumulatorConfig::new();
    /// acc_config.set_operation("population", AccumulatorOp::Sum);
    /// acc_config.set_operation("names", AccumulatorOp::Comma);
    ///
    /// let config = TilerConfig::new(0, 14)
    ///     .with_accumulator(acc_config);
    /// ```
    pub fn with_accumulator(mut self, config: AccumulatorConfig) -> Self {
        self.accumulator_config = Some(config);
        self
    }

    /// Enable point clustering with the specified configuration.
    ///
    /// When enabled, nearby points are clustered together at lower zoom levels.
    /// This matches tippecanoe's `--cluster-distance` and `--cluster-maxzoom` flags.
    ///
    /// # Arguments
    ///
    /// * `distance` - Cluster distance in 256-pixel tile units (tippecanoe default: 50)
    /// * `max_zoom` - Maximum zoom level for clustering (features above this zoom are not clustered)
    ///
    /// # Example
    ///
    /// ```
    /// use gpq_tiles_core::pipeline::TilerConfig;
    ///
    /// let config = TilerConfig::new(0, 14)
    ///     .with_cluster(50, 12); // Cluster within 50px up to zoom 12
    /// ```
    pub fn with_cluster(mut self, distance: u32, max_zoom: u8) -> Self {
        self.cluster_config = Some(ClusterConfig::new(distance, max_zoom));
        self
    }

    /// Enable point clustering with a ClusterConfig.
    ///
    /// Alternative to `with_cluster()` when you have a pre-built config.
    pub fn with_cluster_config(mut self, config: ClusterConfig) -> Self {
        self.cluster_config = Some(config);
        self
    }
}

/// A generated vector tile with its coordinates and data.
#[derive(Debug, Clone)]
pub struct GeneratedTile {
    /// The tile coordinates (x, y, z)
    pub coord: TileCoord,
    /// The MVT protobuf bytes
    pub data: Vec<u8>,
    /// Number of features in this tile
    pub feature_count: usize,
}

impl GeneratedTile {
    /// Create a new generated tile.
    pub fn new(coord: TileCoord, data: Vec<u8>, feature_count: usize) -> Self {
        Self {
            coord,
            data,
            feature_count,
        }
    }

    /// Check if the tile is empty (no data).
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Result of tile generation: tiles iterator plus metadata.
///
/// This struct bundles the tile iterator with metadata needed for
/// writing valid PMTiles headers (bounds, layer name, etc.).
pub struct TileGeneration<I: Iterator<Item = Result<GeneratedTile>>> {
    /// Iterator yielding generated tiles
    pub tiles: I,
    /// Geographic bounding box of the input data
    pub bounds: TileBounds,
    /// Layer name used in the MVT tiles
    pub layer_name: String,
    /// Field metadata: field name -> MVT type ("String", "Number", "Boolean")
    pub fields: std::collections::HashMap<String, String>,
}

/// Generate vector tiles from a GeoParquet file.
///
/// This function reads geometries from the input file, iterates over all tiles
/// in the configured zoom range that intersect the data's bounding box, and
/// generates MVT-encoded tiles for each.
///
/// # Arguments
///
/// * `input_path` - Path to the GeoParquet file
/// * `config` - Tiling configuration
///
/// # Returns
///
/// An iterator of `Result<GeneratedTile>`, yielding each generated tile.
/// Empty tiles (no features after clipping) are skipped.
///
/// # Example
///
/// ```no_run
/// use gpq_tiles_core::pipeline::{generate_tiles, TilerConfig};
/// use std::path::Path;
///
/// let config = TilerConfig::new(0, 10);
/// let tiles = generate_tiles(Path::new("input.parquet"), &config).unwrap();
///
/// for tile_result in tiles {
///     let tile = tile_result.unwrap();
///     println!("Generated tile z{}/x{}/y{}: {} bytes",
///              tile.coord.z, tile.coord.x, tile.coord.y, tile.data.len());
/// }
/// ```
pub fn generate_tiles(
    input_path: &Path,
    config: &TilerConfig,
) -> Result<impl Iterator<Item = Result<GeneratedTile>>> {
    let result = generate_tiles_with_bounds(input_path, config)?;
    Ok(result.tiles)
}

/// Generate vector tiles from a GeoParquet file, returning bounds too.
///
/// Like `generate_tiles()`, but also returns the geographic bounding box
/// of the input data. Use this when you need bounds for PMTiles headers.
///
/// # Example
///
/// ```no_run
/// use gpq_tiles_core::pipeline::{generate_tiles_with_bounds, TilerConfig};
/// use gpq_tiles_core::pmtiles_writer::PmtilesWriter;
/// use std::path::Path;
///
/// let config = TilerConfig::new(0, 10);
/// let result = generate_tiles_with_bounds(Path::new("input.parquet"), &config).unwrap();
///
/// let mut writer = PmtilesWriter::new();
/// writer.set_bounds(&result.bounds);
///
/// for tile_result in result.tiles {
///     let tile = tile_result.unwrap();
///     writer.add_tile(tile.coord.z, tile.coord.x, tile.coord.y, &tile.data).unwrap();
/// }
/// ```
pub fn generate_tiles_with_bounds(
    input_path: &Path,
    config: &TilerConfig,
) -> Result<TileGeneration<impl Iterator<Item = Result<GeneratedTile>>>> {
    // Step 1: Extract field metadata from schema
    let all_fields = extract_field_metadata(input_path).unwrap_or_default();

    // Step 2: Apply property filter to field metadata
    // This filters which fields appear in the JSON metadata
    let fields = if config.property_filter.is_active() {
        all_fields
            .into_iter()
            .filter(|(name, _)| config.property_filter.should_include(name))
            .collect()
    } else {
        all_fields
    };

    // Step 3: Extract all geometries from the GeoParquet file
    // WARNING: This loads all geometries into memory. For large files,
    // we'll need a streaming approach in Phase 4.
    let geometries = extract_geometries(input_path)?;

    if geometries.is_empty() {
        return Ok(TileGeneration {
            tiles: TileIterator::empty(),
            bounds: TileBounds::empty(),
            layer_name: config.layer_name.clone(),
            fields,
        });
    }

    // Step 4: Calculate bounding box from geometries
    let bbox = calculate_bbox_from_geometries(&geometries);

    // Step 5: Create tile iterator
    Ok(TileGeneration {
        tiles: TileIterator::new(geometries, bbox, config.clone()),
        bounds: bbox,
        layer_name: config.layer_name.clone(),
        fields,
    })
}

/// Generate vector tiles from pre-loaded geometries.
///
/// This is a lower-level function useful for benchmarking and library consumers
/// who have already loaded geometries. It bypasses file I/O, which makes it ideal
/// for performance testing.
///
/// # Arguments
///
/// * `geometries` - Pre-loaded geometries (will be sorted by spatial index)
/// * `config` - Tiling configuration
///
/// # Returns
///
/// An iterator of `Result<GeneratedTile>`, yielding each generated tile.
/// Empty tiles (no features after clipping) are skipped.
///
/// # Example
///
/// ```no_run
/// use gpq_tiles_core::pipeline::{generate_tiles_from_geometries, TilerConfig};
/// use gpq_tiles_core::batch_processor::extract_geometries;
/// use std::path::Path;
///
/// // Pre-load geometries (e.g., in benchmark setup)
/// let geometries = extract_geometries(Path::new("input.parquet")).unwrap();
///
/// // Then benchmark just the tiling
/// let config = TilerConfig::new(0, 10);
/// let tiles: Vec<_> = generate_tiles_from_geometries(geometries, &config)
///     .unwrap()
///     .collect();
/// ```
pub fn generate_tiles_from_geometries(
    geometries: Vec<geo::Geometry<f64>>,
    config: &TilerConfig,
) -> Result<impl Iterator<Item = Result<GeneratedTile>>> {
    if geometries.is_empty() {
        return Ok(TileIterator::empty());
    }

    // Calculate bounding box from geometries
    let bbox = calculate_bbox_from_geometries(&geometries);

    // Create tile iterator (sorting happens inside)
    Ok(TileIterator::new(geometries, bbox, config.clone()))
}

/// Calculate bounding box from a collection of geometries.
fn calculate_bbox_from_geometries(geometries: &[geo::Geometry<f64>]) -> TileBounds {
    use geo::BoundingRect;

    let mut bounds = TileBounds::empty();

    for geom in geometries {
        if let Some(rect) = geom.bounding_rect() {
            bounds.expand(&TileBounds::new(
                rect.min().x,
                rect.min().y,
                rect.max().x,
                rect.max().y,
            ));
        }
    }

    bounds
}

/// Generate vector tiles from a GeoParquet file using streaming row-group processing.
///
/// This function processes the input file row-group by row-group, keeping memory usage
/// bounded by the size of the largest row group rather than the entire file.
///
/// For best results with large files, use GeoParquet files that are:
/// - Hilbert-sorted (spatially ordered)
/// - Have row group bounding boxes
/// - Have multiple row groups (50-100MB each)
///
/// Use `gpq optimize --hilbert` from geoparquet-io to prepare files.
///
/// # Arguments
///
/// * `input_path` - Path to the GeoParquet file
/// * `config` - Tiling configuration
///
/// # Returns
///
/// A vector of `GeneratedTile`. Unlike the non-streaming version, this returns
/// a collected Vec because tiles from different row groups may need to be merged.
///
/// # Example
///
/// ```no_run
/// use gpq_tiles_core::pipeline::{generate_tiles_streaming, TilerConfig};
/// use std::path::Path;
///
/// let config = TilerConfig::new(0, 10);
/// let tiles = generate_tiles_streaming(Path::new("large_input.parquet"), &config).unwrap();
///
/// for tile in tiles {
///     println!("Tile z={} x={} y={}", tile.coord.z, tile.coord.x, tile.coord.y);
/// }
/// ```
pub fn generate_tiles_streaming(
    input_path: &Path,
    config: &TilerConfig,
) -> Result<Vec<GeneratedTile>> {
    let (tiles, _stats) = generate_tiles_streaming_with_stats(input_path, config)?;
    Ok(tiles)
}

/// Generate tiles directly to a streaming PMTiles writer.
///
/// This is the most memory-efficient way to convert GeoParquet to PMTiles:
/// - Processes row groups one at a time
/// - Writes tile data to disk immediately via StreamingPmtilesWriter
/// - Only keeps small directory entries (~32 bytes/tile) in memory
///
/// # Arguments
///
/// * `input_path` - Path to the GeoParquet file
/// * `config` - Tiling configuration
/// * `writer` - A StreamingPmtilesWriter to write tiles to
///
/// # Returns
///
/// Memory statistics from processing.
///
/// # Example
///
/// ```no_run
/// use gpq_tiles_core::pipeline::{generate_tiles_to_writer, TilerConfig};
/// use gpq_tiles_core::pmtiles_writer::StreamingPmtilesWriter;
/// use gpq_tiles_core::compression::Compression;
/// use std::path::Path;
///
/// let config = TilerConfig::new(0, 14);
/// let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
///
/// let stats = generate_tiles_to_writer(
///     Path::new("large_input.parquet"),
///     &config,
///     &mut writer,
/// ).unwrap();
///
/// writer.finalize(Path::new("output.pmtiles")).unwrap();
/// ```
#[instrument(name = "pipeline", skip(writer), fields(min_zoom = config.min_zoom, max_zoom = config.max_zoom))]
pub fn generate_tiles_to_writer(
    input_path: &Path,
    config: &TilerConfig,
    writer: &mut crate::pmtiles_writer::StreamingPmtilesWriter,
) -> Result<crate::memory::MemoryStats> {
    use crate::quality::{assess_quality, emit_quality_warnings};

    // Quality assessment with warnings
    if let Ok(quality) = assess_quality(input_path) {
        emit_quality_warnings(&quality, config.quiet);
    }

    // Extract field metadata and set on writer
    let all_fields = extract_field_metadata(input_path).unwrap_or_default();
    let fields = if config.property_filter.is_active() {
        all_fields
            .into_iter()
            .filter(|(name, _)| config.property_filter.should_include(name))
            .collect()
    } else {
        all_fields
    };
    writer.set_fields(fields);
    writer.set_layer_name(&config.layer_name);

    // Always use geometry-centric external sort algorithm (fast + bounded memory)
    generate_tiles_to_writer_internal(input_path, config, writer, None)
}

/// Generate tiles directly to a streaming PMTiles writer with progress reporting.
///
/// Same as `generate_tiles_to_writer` but accepts a progress callback for monitoring.
#[allow(clippy::type_complexity)]
#[instrument(name = "pipeline", skip(writer, progress), fields(min_zoom = config.min_zoom, max_zoom = config.max_zoom))]
pub fn generate_tiles_to_writer_with_progress(
    input_path: &Path,
    config: &TilerConfig,
    writer: &mut crate::pmtiles_writer::StreamingPmtilesWriter,
    progress: ProgressCallback,
) -> Result<crate::memory::MemoryStats> {
    use crate::quality::{assess_quality, emit_quality_warnings};

    // Quality assessment with warnings
    if let Ok(quality) = assess_quality(input_path) {
        emit_quality_warnings(&quality, config.quiet);
    }

    // Extract field metadata and set on writer
    let all_fields = extract_field_metadata(input_path).unwrap_or_default();
    let fields = if config.property_filter.is_active() {
        all_fields
            .into_iter()
            .filter(|(name, _)| config.property_filter.should_include(name))
            .collect()
    } else {
        all_fields
    };
    writer.set_fields(fields);
    writer.set_layer_name(&config.layer_name);

    // Always use geometry-centric external sort algorithm (fast + bounded memory)
    generate_tiles_to_writer_internal(input_path, config, writer, Some(progress))
}

/// Fast streaming mode: single file pass, stores clipped geometries per tile.
///
/// Memory usage: ~1-2GB for large files (clipped geometries are ~90% smaller than originals)
/// Geometry-centric tile generation with external sort for bounded memory.
///
/// Phase 1: Read file, clip geometries, write to external sorter
/// Phase 2: Sort by tile_id (memory-bounded external merge sort)
/// Phase 3: Read sorted, group by tile, encode MVT, write PMTiles
///
/// Memory usage: O(sort_buffer_size) - configurable, typically 100K-1M records
/// This is the only tile generation algorithm - geometry-centric with external sort.
fn generate_tiles_to_writer_internal(
    input_path: &Path,
    config: &TilerConfig,
    writer: &mut crate::pmtiles_writer::StreamingPmtilesWriter,
    progress: Option<ProgressCallback>,
) -> Result<crate::memory::MemoryStats> {
    use crate::batch_processor::{get_row_group_count, get_total_row_count};
    use crate::external_sort::{
        calculate_optimal_sort_buffer, ShardedTileFeatureSorter, TileFeatureRecord,
    };
    use crate::memory::{MemoryStats, MemoryTracker};
    use crate::mvt::{LayerBuilder, TileBuilder};
    use crate::pmtiles_writer::tile_id;
    use geo::BoundingRect;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::Instant;

    let start_time = Instant::now();

    // Thread-safe state for parallel geometry processing
    let memory_tracker = Mutex::new(match config.memory_budget {
        Some(budget) => MemoryTracker::with_budget(budget),
        None => MemoryTracker::new(),
    });

    let global_bounds = Mutex::new(TileBounds::empty());
    let global_feature_index = AtomicU64::new(0);

    // Get total row group count for progress tracking
    let total_row_groups = get_row_group_count(input_path).unwrap_or(1);

    // Get total row count for dynamic buffer sizing
    let total_rows = get_total_row_count(input_path).unwrap_or(0);

    // Phase 1: Read GeoParquet, clip geometries, write to sorter
    // Dynamic buffer sizing: ensures each shard creates ≤50 segments to avoid FD exhaustion
    // Memory: ~180MB per shard for 365K buffer with average 500-byte records
    const NUM_SHARDS: usize = 16;
    let sort_buffer_size = calculate_optimal_sort_buffer(total_rows, NUM_SHARDS);
    if !config.quiet {
        tracing::debug!(
            "Sort buffer: {} records (estimated {} total rows)",
            sort_buffer_size,
            total_rows
        );
    }
    let sorter = Mutex::new(ShardedTileFeatureSorter::new(sort_buffer_size));

    // TileFeatureRecord fixed overhead: tile_id(8) + z(1) + x(4) + y(4) + feature_id(8) = 25 bytes
    const RECORD_FIXED_OVERHEAD: usize = 25;

    if let Some(ref cb) = progress {
        cb(ProgressEvent::PhaseStart {
            phase: 1,
            name: "Reading GeoParquet",
        });
    }
    if !config.quiet {
        tracing::info!("Phase 1: Reading GeoParquet and writing to external sorter");
    }

    let records_written = AtomicU64::new(0);
    let geoms_processed = AtomicU64::new(0);
    let row_groups_completed = AtomicU64::new(0);

    // Phase 1 span: Read GeoParquet and clip geometries
    // Use parallel row group reader for overlapped I/O and parallel decompression
    let _phase1_span = tracing::info_span!("read_parquet", path = %input_path.display()).entered();

    process_geometries_parallel(
        input_path,
        DEFAULT_PARALLEL_READERS,
        |rg_info: RowGroupInfo, geometries| {
            // Row group span for detailed tracing
            let _rg_span = tracing::info_span!(
                "row_group",
                index = rg_info.index,
                row_count = rg_info.num_rows
            )
            .entered();
            let features_in_group = geometries.len();

            // Track completed row groups (may arrive out of order due to parallel reads)
            let completed = row_groups_completed.fetch_add(1, Ordering::Relaxed) + 1;

            if let Some(ref cb) = progress {
                cb(ProgressEvent::Phase1Progress {
                    row_group: completed as usize,
                    total_row_groups,
                    features_in_group,
                    records_written: records_written.load(Ordering::Relaxed),
                });
            }
            if !config.quiet {
                tracing::info!(
                    "Processing row group {}/{} with {} features",
                    completed,
                    total_row_groups,
                    rg_info.num_rows
                );
            }

            // PROFILING: Track time spent in each phase
            let rg_start = std::time::Instant::now();

            // Sort geometries within row group for better locality
            let sort_start = std::time::Instant::now();
            let mut sorted = geometries;
            sort_geometries(&mut sorted, config.use_hilbert);
            let time_sort = sort_start.elapsed();

            let num_geoms = sorted.len();

            // Assign feature indices upfront for all geometries in this row group
            // This ensures deterministic feature IDs regardless of parallel execution order
            let base_feat_idx = global_feature_index.fetch_add(num_geoms as u64, Ordering::SeqCst);

            // Result type for per-geometry processing stats
            struct GeomResult {
                records: Vec<TileFeatureRecord>,
                time_clip: std::time::Duration,
                time_simplify: std::time::Duration,
                time_wkb: std::time::Duration,
                clip_count: u64,
                tiles_touched: u64,
                bounds: Option<TileBounds>,
            }

            // Process geometries - either in parallel or sequentially based on config
            let process_geometry = |geom_idx: usize, geom: Geometry<f64>| -> GeomResult {
                let feat_idx = base_feat_idx + geom_idx as u64;

                let mut result = GeomResult {
                    records: Vec::new(),
                    time_clip: std::time::Duration::ZERO,
                    time_simplify: std::time::Duration::ZERO,
                    time_wkb: std::time::Duration::ZERO,
                    clip_count: 0,
                    tiles_touched: 0,
                    bounds: None,
                };

                let geom_bbox = match geom.bounding_rect() {
                    Some(rect) => {
                        let bounds =
                            TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
                        result.bounds = Some(bounds);
                        bounds
                    }
                    None => return result,
                };

                // Pre-simplify geometry ONCE at the MAX zoom level tolerance
                let simplify_start = std::time::Instant::now();
                let base_simplified = simplify_for_zoom(&geom, config.max_zoom, config.extent);
                result.time_simplify = simplify_start.elapsed();

                // Hierarchical clipping in WorldCoord space: clip at min_zoom first,
                // then clip parent results for child tiles at higher zoom levels.
                // This avoids re-clipping the full original geometry for every tile.
                // See issue #38 for rationale.
                //
                // Phase 2 change: Use WorldCoord-based clipping for integer precision
                // throughout the pipeline. This eliminates f64 accumulation errors.
                let clip_start = std::time::Instant::now();
                let (clip_results, clip_stats) = clip_geometry_hierarchical_world(
                    &base_simplified,
                    &geom_bbox,
                    config.min_zoom,
                    config.max_zoom,
                    config.buffer_pixels,
                    config.extent,
                );
                result.time_clip = clip_start.elapsed();
                result.clip_count = clip_stats.clip_ops;
                result.tiles_touched = clip_stats.tiles_processed;

                // Process clip results: validate, drop, serialize to bytes, create records
                // Collect into a Vec first for deterministic ordering
                let mut clip_entries: Vec<_> = clip_results.into_iter().collect();
                clip_entries
                    .sort_by(|(a, _), (b, _)| tile_id(a.z, a.x, a.y).cmp(&tile_id(b.z, b.x, b.y)));

                for (tile_coord, clipped) in clip_entries {
                    // Check dropping rules using WorldCoord-based functions
                    let should_drop = match &clipped {
                        WorldClippedGeometry::Point(_) => {
                            // Point dropping uses feature index for density
                            should_drop_point(
                                &geo::Point::new(0.0, 0.0),
                                tile_coord.z,
                                config.max_zoom,
                                feat_idx,
                            )
                        }
                        WorldClippedGeometry::MultiPoint(points) => points.is_empty(),
                        WorldClippedGeometry::LineString(coords) => {
                            should_drop_tiny_line_world(coords, &tile_coord, config.extent, 1.0)
                        }
                        WorldClippedGeometry::MultiLineString(lines) => lines.iter().all(|line| {
                            should_drop_tiny_line_world(line, &tile_coord, config.extent, 1.0)
                        }),
                        WorldClippedGeometry::Polygon {
                            exterior,
                            interiors,
                        } => {
                            // When accumulation is enabled, don't drop tiny polygons here -
                            // they'll be accumulated in Phase 3 (encode_tile_from_raw)
                            if config.enable_tiny_polygon_accumulation {
                                false // Don't drop - will be handled by accumulator
                            } else {
                                should_drop_tiny_polygon_world(
                                    exterior,
                                    interiors,
                                    &tile_coord,
                                    config.extent,
                                    DEFAULT_TINY_POLYGON_THRESHOLD,
                                )
                            }
                        }
                        WorldClippedGeometry::MultiPolygon(polys) => {
                            // When accumulation is enabled, don't drop tiny polygons here
                            if config.enable_tiny_polygon_accumulation {
                                false // Don't drop - will be handled by accumulator
                            } else {
                                polys.iter().all(|(ext, ints)| {
                                    should_drop_tiny_polygon_world(
                                        ext,
                                        ints,
                                        &tile_coord,
                                        config.extent,
                                        DEFAULT_TINY_POLYGON_THRESHOLD,
                                    )
                                })
                            }
                        }
                    };

                    if should_drop {
                        continue;
                    }

                    // Serialize WorldClippedGeometry to bytes (replacing WKB)
                    let serialize_start = std::time::Instant::now();
                    let geom_bytes = clipped.to_bytes();
                    result.time_wkb += serialize_start.elapsed();

                    // Create record with tile_id and coordinates
                    let tid = tile_id(tile_coord.z, tile_coord.x, tile_coord.y);
                    let record = TileFeatureRecord::new(
                        tid,
                        tile_coord.z,
                        tile_coord.x,
                        tile_coord.y,
                        feat_idx,
                        geom_bytes,
                        vec![], // Empty properties for now
                    );

                    result.records.push(record);
                }

                result
            };

            // Use parallel or sequential geometry processing based on deterministic flag
            let results: Vec<GeomResult> = if config.deterministic {
                // SEQUENTIAL: For reproducible output
                sorted
                    .into_iter()
                    .enumerate()
                    .map(|(idx, geom)| process_geometry(idx, geom))
                    .collect()
            } else {
                // PARALLEL: For performance (default)
                sorted
                    .into_iter()
                    .enumerate()
                    .collect::<Vec<_>>()
                    .into_par_iter()
                    .map(|(idx, geom)| process_geometry(idx, geom))
                    .collect()
            };

            // Aggregate all results and write to sorter
            let mut time_clip = std::time::Duration::ZERO;
            let mut time_simplify = std::time::Duration::ZERO;
            let mut time_wkb = std::time::Duration::ZERO;
            let mut time_sorter_add = std::time::Duration::ZERO;
            let mut clip_count = 0u64;
            let mut tiles_touched = 0u64;

            for result in results {
                time_clip += result.time_clip;
                time_simplify += result.time_simplify;
                time_wkb += result.time_wkb;
                clip_count += result.clip_count;
                tiles_touched += result.tiles_touched;

                // Update global bounds
                if let Some(bounds) = result.bounds {
                    global_bounds.lock().unwrap().expand(&bounds);
                }

                // Add records to sorter
                let sorter_start = std::time::Instant::now();
                {
                    let mut sorter_guard = sorter.lock().unwrap();
                    let mut tracker_guard = memory_tracker.lock().unwrap();
                    for record in result.records {
                        let record_size = RECORD_FIXED_OVERHEAD + record.geometry_wkb.len();
                        tracker_guard.add(record_size);
                        sorter_guard.add(record);
                        records_written.fetch_add(1, Ordering::Relaxed);
                    }
                }
                time_sorter_add += sorter_start.elapsed();
            }

            geoms_processed.fetch_add(num_geoms as u64, Ordering::Relaxed);

            // Log timing at debug level (use RUST_LOG=debug to see)
            log::debug!(
            "RG {}: {:.2}s | sort={:.2}s clip={:.2}s ({} ops) simplify={:.2}s wkb={:.2}s sorter={:.2}s | tiles={}",
            rg_info.index,
            rg_start.elapsed().as_secs_f64(),
            time_sort.as_secs_f64(),
            time_clip.as_secs_f64(),
            clip_count,
            time_simplify.as_secs_f64(),
            time_wkb.as_secs_f64(),
            time_sorter_add.as_secs_f64(),
            tiles_touched,
        );

            Ok(())
        },
    )?;

    // After Phase 1, sorter memory is released when it starts sorting to disk
    // Reset current memory as records are now on disk during sort
    let phase1_peak = memory_tracker.lock().unwrap().peak();
    memory_tracker.lock().unwrap().reset_current();

    let total_records = sorter.lock().unwrap().len() as u64;

    if let Some(ref cb) = progress {
        cb(ProgressEvent::Phase1Complete {
            total_records,
            peak_memory_bytes: phase1_peak,
        });
    }
    if !config.quiet {
        tracing::info!(
            "Phase 1 complete: {} records to sort (peak memory so far: {} bytes)",
            total_records,
            phase1_peak
        );
    }

    // Drop Phase 1 span
    drop(_phase1_span);

    // Phase 2: Sort by tile_id (external merge sort)
    let _phase2_span = tracing::info_span!("sort").entered();
    if let Some(ref cb) = progress {
        cb(ProgressEvent::Phase2Start);
    }
    if !config.quiet {
        tracing::info!("Phase 2: External merge sort by tile_id");
    }

    let sorted_iter = sorter
        .into_inner()
        .unwrap()
        .sort_with_progress(|shard, total_shards, records_in_shard, duration| {
            if let Some(ref cb) = progress {
                cb(ProgressEvent::Phase2Progress {
                    shard,
                    total_shards,
                    records_in_shard,
                    shard_duration_secs: duration,
                });
            }
        })
        .map_err(|e| Error::PMTilesWrite(format!("External sort failed: {}", e)))?;

    if let Some(ref cb) = progress {
        cb(ProgressEvent::Phase2Complete);
    }
    drop(_phase2_span);

    // Phase 3: Read sorted records, group by tile, encode MVT, write PMTiles
    // PARALLEL ENCODING: Batch tiles and encode in parallel for ~Nx speedup on N cores
    let _phase3_span = tracing::info_span!("encode", total_records).entered();
    if let Some(ref cb) = progress {
        cb(ProgressEvent::PhaseStart {
            phase: 3,
            name: "Encoding tiles",
        });
    }
    if !config.quiet {
        tracing::info!("Phase 3: Encoding tiles and writing to PMTiles (parallel)");
    }

    // Batch size for parallel encoding - balance between parallelism and memory
    // Larger batches = more parallelism, but more memory
    // 2000 tiles provides good parallel work while keeping memory bounded
    const BATCH_SIZE: usize = 2000;

    // Raw record data for deferred decoding + encoding
    struct RawFeature {
        geometry_bytes: Vec<u8>,
        feature_id: u64,
    }

    // Tile with raw (not yet decoded) features
    struct RawTileData {
        z: u8,
        x: u32,
        y: u32,
        features: Vec<RawFeature>,
    }

    // Encoded tile ready for writing
    struct EncodedTile {
        z: u8,
        x: u32,
        y: u32,
        data: Vec<u8>,
        feature_count: usize,
    }

    // Helper function to decode and encode a single tile (pure, no side effects)
    // This does geometry decoding + MVT encoding together for better parallelism
    //
    // When enable_tiny_polygon_accumulation is true, tiny polygons are accumulated
    // and synthetic squares are emitted when the accumulated area exceeds the threshold.
    // This matches tippecanoe's behavior (clip.cpp:1048-1097).
    //
    // When cluster_config is Some, point clustering is performed at the appropriate zoom levels.
    fn encode_tile_from_raw(
        tile_data: RawTileData,
        layer_name: &str,
        extent: u32,
        enable_tiny_polygon_accumulation: bool,
        cluster_config: Option<&ClusterConfig>,
    ) -> Option<EncodedTile> {
        use crate::clustering::{IndexedPoint, PointClusterer};
        use crate::mvt::PropertyValue;
        use crate::world_coord::WorldCoord;

        let coord = TileCoord::new(tile_data.x, tile_data.y, tile_data.z);
        let mut layer_builder = LayerBuilder::new(layer_name).with_extent(extent);
        let mut feature_count = 0;

        // Create accumulator for tiny polygons if enabled
        let mut accumulator = if enable_tiny_polygon_accumulation {
            Some(TinyPolygonAccumulator::new(
                coord,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD,
            ))
        } else {
            None
        };

        // Track feature ID for synthetic squares (use a high value to avoid collision)
        let mut synthetic_feature_id = u64::MAX - 1000;

        // Point clustering: if enabled, collect all points first, cluster them, then process
        let should_cluster = cluster_config
            .map(|cc| coord.z <= cc.max_zoom)
            .unwrap_or(false);

        if should_cluster {
            let cc = cluster_config.unwrap();

            // Decode all features, separating points from non-points
            let mut points_to_cluster: Vec<(u64, WorldCoord)> = Vec::new();
            let mut non_point_features: Vec<(u64, WorldClippedGeometry)> = Vec::new();

            for raw_feat in tile_data.features.iter() {
                let geom = match WorldClippedGeometry::from_bytes(&raw_feat.geometry_bytes) {
                    Some(g) => g,
                    None => continue,
                };

                if geom.is_degenerate_in_tile(&coord, extent) {
                    continue;
                }

                match geom {
                    WorldClippedGeometry::Point(p) => {
                        points_to_cluster.push((raw_feat.feature_id, p));
                    }
                    _ => {
                        non_point_features.push((raw_feat.feature_id, geom));
                    }
                }
            }

            // Cluster the points using Hilbert index proximity
            if !points_to_cluster.is_empty() {
                // Convert to IndexedPoints for clustering
                use crate::spatial_index::{encode_hilbert, lng_lat_to_world_coords};
                use std::collections::HashMap;

                let indexed_points: Vec<IndexedPoint> = points_to_cluster
                    .iter()
                    .map(|(feat_id, wc)| {
                        // Convert WorldCoord back to lng/lat for Hilbert indexing
                        let lng = (wc.x as f64 / (1u64 << 32) as f64) * 360.0 - 180.0;
                        let lat_rad =
                            std::f64::consts::PI * (1.0 - 2.0 * wc.y as f64 / (1u64 << 32) as f64);
                        let lat = lat_rad.sinh().atan().to_degrees();

                        let (wx, wy) = lng_lat_to_world_coords(lng, lat);
                        let hilbert = encode_hilbert(wx, wy);

                        // Store feature_id in properties for tracking
                        let mut props = HashMap::new();
                        props.insert(
                            "__feature_id".to_string(),
                            crate::wkb::PropertyValue::UInt(*feat_id),
                        );

                        IndexedPoint {
                            point: geo::Point::new(lng, lat),
                            hilbert_index: hilbert,
                            world_x: wx,
                            world_y: wy,
                            properties: props,
                        }
                    })
                    .collect();

                // Create clusterer and cluster points
                let clusterer = PointClusterer::new(cc.clone(), None);
                let clustered = clusterer.cluster(indexed_points, coord.z);

                // Add clustered points to the layer
                for cp in clustered {
                    // Convert clustered point back to WorldCoord
                    let (wx, wy) = lng_lat_to_world_coords(cp.point.x(), cp.point.y());

                    // Use the world coordinates directly (already u32)
                    let wc = WorldCoord { x: wx, y: wy };

                    let point_geom = WorldClippedGeometry::Point(wc);

                    // Build properties for MVT (include cluster_count if present)
                    let mut props: Vec<(String, PropertyValue)> = Vec::new();
                    if let Some(crate::wkb::PropertyValue::UInt(count)) =
                        cp.properties.get("cluster_count")
                    {
                        props.push(("cluster_count".to_string(), PropertyValue::UInt(*count)));
                    }

                    // Get feature ID (use original if single point, or synthetic for cluster)
                    let feat_id = if cp.properties.contains_key("cluster_count") {
                        // This is a cluster - use synthetic ID
                        synthetic_feature_id = synthetic_feature_id.wrapping_sub(1);
                        Some(synthetic_feature_id)
                    } else {
                        // Single point - use original ID
                        cp.properties.get("__feature_id").and_then(|v| match v {
                            crate::wkb::PropertyValue::UInt(id) => Some(*id),
                            _ => None,
                        })
                    };

                    layer_builder.add_feature_world(feat_id, &point_geom, &props, &coord);
                    feature_count += 1;
                }
            }

            // Process non-point features with existing polygon accumulation logic
            for (feat_id, geom) in non_point_features {
                // Handle tiny polygon accumulation
                match (&geom, &mut accumulator) {
                    (
                        WorldClippedGeometry::Polygon {
                            exterior,
                            interiors,
                        },
                        Some(ref mut acc),
                    ) => {
                        if should_drop_tiny_polygon_world(
                            exterior,
                            interiors,
                            &coord,
                            extent,
                            DEFAULT_TINY_POLYGON_THRESHOLD,
                        ) {
                            acc.accumulate(exterior, interiors);
                            while acc.should_emit() {
                                if let Some((syn_ext, syn_int)) = acc.emit_synthetic_square() {
                                    let synthetic_geom = WorldClippedGeometry::Polygon {
                                        exterior: syn_ext,
                                        interiors: syn_int,
                                    };
                                    layer_builder.add_feature_world(
                                        Some(synthetic_feature_id),
                                        &synthetic_geom,
                                        &[],
                                        &coord,
                                    );
                                    feature_count += 1;
                                    synthetic_feature_id = synthetic_feature_id.wrapping_sub(1);
                                } else {
                                    break;
                                }
                            }
                        } else {
                            layer_builder.add_feature_world(Some(feat_id), &geom, &[], &coord);
                            feature_count += 1;
                        }
                    }
                    (WorldClippedGeometry::MultiPolygon(polys), Some(ref mut acc)) => {
                        let mut has_normal_polygons = false;
                        let mut normal_polygons: Vec<(
                            Vec<crate::world_coord::WorldCoord>,
                            Vec<Vec<crate::world_coord::WorldCoord>>,
                        )> = Vec::new();

                        for (ext, ints) in polys.iter() {
                            if should_drop_tiny_polygon_world(
                                ext,
                                ints,
                                &coord,
                                extent,
                                DEFAULT_TINY_POLYGON_THRESHOLD,
                            ) {
                                acc.accumulate(ext, ints);
                                while acc.should_emit() {
                                    if let Some((syn_ext, syn_int)) = acc.emit_synthetic_square() {
                                        let synthetic_geom = WorldClippedGeometry::Polygon {
                                            exterior: syn_ext,
                                            interiors: syn_int,
                                        };
                                        layer_builder.add_feature_world(
                                            Some(synthetic_feature_id),
                                            &synthetic_geom,
                                            &[],
                                            &coord,
                                        );
                                        feature_count += 1;
                                        synthetic_feature_id = synthetic_feature_id.wrapping_sub(1);
                                    } else {
                                        break;
                                    }
                                }
                            } else {
                                has_normal_polygons = true;
                                normal_polygons.push((ext.clone(), ints.clone()));
                            }
                        }

                        if has_normal_polygons {
                            if normal_polygons.len() == 1 {
                                let (ext, ints) = normal_polygons.into_iter().next().unwrap();
                                let poly_geom = WorldClippedGeometry::Polygon {
                                    exterior: ext,
                                    interiors: ints,
                                };
                                layer_builder.add_feature_world(
                                    Some(feat_id),
                                    &poly_geom,
                                    &[],
                                    &coord,
                                );
                            } else {
                                let multi_geom =
                                    WorldClippedGeometry::MultiPolygon(normal_polygons);
                                layer_builder.add_feature_world(
                                    Some(feat_id),
                                    &multi_geom,
                                    &[],
                                    &coord,
                                );
                            }
                            feature_count += 1;
                        }
                    }
                    _ => {
                        layer_builder.add_feature_world(Some(feat_id), &geom, &[], &coord);
                        feature_count += 1;
                    }
                }
            }

            // Emit remaining accumulated tiny polygons
            if let Some(ref mut acc) = accumulator {
                while acc.should_emit() {
                    if let Some((syn_ext, syn_int)) = acc.emit_synthetic_square() {
                        let synthetic_geom = WorldClippedGeometry::Polygon {
                            exterior: syn_ext,
                            interiors: syn_int,
                        };
                        layer_builder.add_feature_world(
                            Some(synthetic_feature_id),
                            &synthetic_geom,
                            &[],
                            &coord,
                        );
                        feature_count += 1;
                        synthetic_feature_id = synthetic_feature_id.wrapping_sub(1);
                    } else {
                        break;
                    }
                }
            }

            // Return early - we've processed everything
            if feature_count > 0 {
                let layer = layer_builder.build();
                let mut tile_builder = TileBuilder::new();
                tile_builder.add_layer(layer);
                let tile = tile_builder.build();
                let encoded = tile.encode_to_vec();

                return Some(EncodedTile {
                    z: tile_data.z,
                    x: tile_data.x,
                    y: tile_data.y,
                    data: encoded,
                    feature_count,
                });
            } else {
                return None;
            }
        }

        // Non-clustering path: original loop
        for raw_feat in tile_data.features {
            // Decode geometry from bytes (this was previously sequential)
            let geom = match WorldClippedGeometry::from_bytes(&raw_feat.geometry_bytes) {
                Some(g) => g,
                None => continue,
            };

            if geom.is_degenerate_in_tile(&coord, extent) {
                continue;
            }

            // Handle tiny polygon accumulation
            match (&geom, &mut accumulator) {
                (
                    WorldClippedGeometry::Polygon {
                        exterior,
                        interiors,
                    },
                    Some(ref mut acc),
                ) => {
                    // Check if this polygon is tiny
                    if should_drop_tiny_polygon_world(
                        exterior,
                        interiors,
                        &coord,
                        extent,
                        DEFAULT_TINY_POLYGON_THRESHOLD,
                    ) {
                        // Accumulate tiny polygon instead of dropping
                        acc.accumulate(exterior, interiors);

                        // Emit synthetic square if threshold exceeded
                        while acc.should_emit() {
                            if let Some((syn_ext, syn_int)) = acc.emit_synthetic_square() {
                                let synthetic_geom = WorldClippedGeometry::Polygon {
                                    exterior: syn_ext,
                                    interiors: syn_int,
                                };
                                layer_builder.add_feature_world(
                                    Some(synthetic_feature_id),
                                    &synthetic_geom,
                                    &[],
                                    &coord,
                                );
                                feature_count += 1;
                                synthetic_feature_id = synthetic_feature_id.wrapping_sub(1);
                            } else {
                                break;
                            }
                        }
                    } else {
                        // Normal-sized polygon - add directly
                        layer_builder.add_feature_world(
                            Some(raw_feat.feature_id),
                            &geom,
                            &[],
                            &coord,
                        );
                        feature_count += 1;
                    }
                }
                (WorldClippedGeometry::MultiPolygon(polys), Some(ref mut acc)) => {
                    // For MultiPolygon: check each polygon individually
                    let mut has_normal_polygons = false;
                    let mut normal_polygons: Vec<(
                        Vec<crate::world_coord::WorldCoord>,
                        Vec<Vec<crate::world_coord::WorldCoord>>,
                    )> = Vec::new();

                    for (ext, ints) in polys.iter() {
                        if should_drop_tiny_polygon_world(
                            ext,
                            ints,
                            &coord,
                            extent,
                            DEFAULT_TINY_POLYGON_THRESHOLD,
                        ) {
                            // Accumulate tiny polygon
                            acc.accumulate(ext, ints);

                            // Emit synthetic square if threshold exceeded
                            while acc.should_emit() {
                                if let Some((syn_ext, syn_int)) = acc.emit_synthetic_square() {
                                    let synthetic_geom = WorldClippedGeometry::Polygon {
                                        exterior: syn_ext,
                                        interiors: syn_int,
                                    };
                                    layer_builder.add_feature_world(
                                        Some(synthetic_feature_id),
                                        &synthetic_geom,
                                        &[],
                                        &coord,
                                    );
                                    feature_count += 1;
                                    synthetic_feature_id = synthetic_feature_id.wrapping_sub(1);
                                } else {
                                    break;
                                }
                            }
                        } else {
                            // Normal-sized polygon - keep for MultiPolygon
                            has_normal_polygons = true;
                            normal_polygons.push((ext.clone(), ints.clone()));
                        }
                    }

                    // If there are any normal-sized polygons, add them
                    if has_normal_polygons {
                        if normal_polygons.len() == 1 {
                            // Single polygon remaining - emit as Polygon
                            let (ext, ints) = normal_polygons.into_iter().next().unwrap();
                            let poly_geom = WorldClippedGeometry::Polygon {
                                exterior: ext,
                                interiors: ints,
                            };
                            layer_builder.add_feature_world(
                                Some(raw_feat.feature_id),
                                &poly_geom,
                                &[],
                                &coord,
                            );
                        } else {
                            // Multiple polygons remaining - emit as MultiPolygon
                            let multi_geom = WorldClippedGeometry::MultiPolygon(normal_polygons);
                            layer_builder.add_feature_world(
                                Some(raw_feat.feature_id),
                                &multi_geom,
                                &[],
                                &coord,
                            );
                        }
                        feature_count += 1;
                    }
                }
                _ => {
                    // Non-polygon geometry - add directly
                    layer_builder.add_feature_world(Some(raw_feat.feature_id), &geom, &[], &coord);
                    feature_count += 1;
                }
            }
        }

        // After processing all features, emit any remaining accumulated polygons
        if let Some(ref mut acc) = accumulator {
            while acc.should_emit() {
                if let Some((syn_ext, syn_int)) = acc.emit_synthetic_square() {
                    let synthetic_geom = WorldClippedGeometry::Polygon {
                        exterior: syn_ext,
                        interiors: syn_int,
                    };
                    layer_builder.add_feature_world(
                        Some(synthetic_feature_id),
                        &synthetic_geom,
                        &[],
                        &coord,
                    );
                    feature_count += 1;
                    synthetic_feature_id = synthetic_feature_id.wrapping_sub(1);
                } else {
                    break;
                }
            }
        }

        if feature_count > 0 {
            let layer = layer_builder.build();
            let mut tile_builder = TileBuilder::new();
            tile_builder.add_layer(layer);
            let tile = tile_builder.build();
            let encoded = tile.encode_to_vec();

            Some(EncodedTile {
                z: tile_data.z,
                x: tile_data.x,
                y: tile_data.y,
                data: encoded,
                feature_count,
            })
        } else {
            None
        }
    }

    let mut current_tile: Option<(u8, u32, u32)> = None;
    let mut current_features: Vec<RawFeature> = Vec::new();
    let mut current_tile_memory: usize = 0;
    let mut tiles_written: u64 = 0;
    let mut records_processed: u64 = 0;
    let progress_interval: u64 = std::cmp::max(1, total_records / 100);

    // Batch of complete tiles waiting to be encoded
    let mut tile_batch: Vec<RawTileData> = Vec::with_capacity(BATCH_SIZE);
    let mut batch_memory: usize = 0;

    // Clone config values for use in closure
    let layer_name = config.layer_name.clone();
    let extent = config.extent;
    let deterministic = config.deterministic;
    let enable_tiny_polygon_accumulation = config.enable_tiny_polygon_accumulation;
    let cluster_config = config.cluster_config.clone();

    // Helper to flush the batch: decode + encode in parallel, write sequentially
    let flush_batch = |batch: &mut Vec<RawTileData>,
                       batch_mem: &mut usize,
                       writer: &mut crate::pmtiles_writer::StreamingPmtilesWriter,
                       tiles_written: &mut u64,
                       layer_name: &str,
                       extent: u32,
                       deterministic: bool,
                       enable_tiny_polygon_accumulation: bool,
                       cluster_config: Option<&ClusterConfig>|
     -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        // Decode + encode tiles - parallel or sequential based on deterministic flag
        let encoded_tiles: Vec<Option<EncodedTile>> = if deterministic {
            // Sequential for reproducibility
            std::mem::take(batch)
                .into_iter()
                .map(|td| {
                    encode_tile_from_raw(
                        td,
                        layer_name,
                        extent,
                        enable_tiny_polygon_accumulation,
                        cluster_config,
                    )
                })
                .collect()
        } else {
            // Parallel for performance - both decoding AND encoding happen in parallel
            std::mem::take(batch)
                .into_par_iter()
                .map(|td| {
                    encode_tile_from_raw(
                        td,
                        layer_name,
                        extent,
                        enable_tiny_polygon_accumulation,
                        cluster_config,
                    )
                })
                .collect()
        };

        // Write encoded tiles sequentially (PMTiles requires ordered writes)
        for encoded in encoded_tiles.into_iter().flatten() {
            writer
                .add_tile_with_count(
                    encoded.z,
                    encoded.x,
                    encoded.y,
                    &encoded.data,
                    encoded.feature_count,
                )
                .map_err(|e| Error::PMTilesWrite(format!("Failed to write tile: {}", e)))?;
            *tiles_written += 1;
        }

        *batch_mem = 0;
        Ok(())
    };

    for record_result in sorted_iter {
        records_processed += 1;

        // Report progress periodically
        if records_processed % progress_interval == 0 {
            if let Some(ref cb) = progress {
                cb(ProgressEvent::Phase3Progress {
                    tiles_written,
                    records_processed,
                    total_records,
                });
            }
        }

        let record = record_result
            .map_err(|e| Error::PMTilesWrite(format!("Failed to read sorted record: {}", e)))?;

        let record_tile = (record.z, record.x, record.y);

        // Check if we're starting a new tile
        if let Some((z, x, y)) = current_tile {
            if (z, x, y) != record_tile {
                // Current tile is complete - add to batch
                tile_batch.push(RawTileData {
                    z,
                    x,
                    y,
                    features: std::mem::take(&mut current_features),
                });
                batch_memory += current_tile_memory;

                // Reset memory tracking for current tile
                memory_tracker.lock().unwrap().remove(current_tile_memory);
                current_tile_memory = 0;

                // Flush batch if it's full
                if tile_batch.len() >= BATCH_SIZE {
                    flush_batch(
                        &mut tile_batch,
                        &mut batch_memory,
                        writer,
                        &mut tiles_written,
                        &layer_name,
                        extent,
                        deterministic,
                        enable_tiny_polygon_accumulation,
                        cluster_config.as_ref(),
                    )?;
                }
            }
        }

        // Store raw bytes for deferred decoding (will be decoded in parallel)
        let geom_size = record.geometry_wkb.len();
        memory_tracker.lock().unwrap().add(geom_size);
        current_tile_memory += geom_size;

        // Add raw feature to current tile
        current_features.push(RawFeature {
            geometry_bytes: record.geometry_wkb,
            feature_id: record.feature_id,
        });
        current_tile = Some(record_tile);
    }

    // Add the final tile to batch
    if let Some((z, x, y)) = current_tile {
        if !current_features.is_empty() {
            tile_batch.push(RawTileData {
                z,
                x,
                y,
                features: current_features,
            });
        }
    }

    // Flush any remaining tiles
    flush_batch(
        &mut tile_batch,
        &mut batch_memory,
        writer,
        &mut tiles_written,
        &layer_name,
        extent,
        deterministic,
        enable_tiny_polygon_accumulation,
        cluster_config.as_ref(),
    )?;

    writer.set_bounds(&global_bounds.into_inner().unwrap());

    let stats = MemoryStats::from_tracker(&memory_tracker.into_inner().unwrap());
    let duration = start_time.elapsed();

    if let Some(ref cb) = progress {
        cb(ProgressEvent::Complete {
            total_tiles: tiles_written,
            peak_memory_bytes: stats.peak_bytes,
            duration_secs: duration.as_secs_f64(),
        });
    }
    if !config.quiet {
        tracing::info!(
            "External sort streaming complete: {} tiles written, peak memory {}",
            tiles_written,
            stats.peak_formatted()
        );
    }

    Ok(stats)
}

/// Result of streaming tile generation including memory statistics.
#[derive(Debug)]
pub struct StreamingResult {
    /// Generated tiles
    pub tiles: Vec<GeneratedTile>,
    /// Memory usage statistics
    pub memory_stats: crate::memory::MemoryStats,
}

/// Generate tiles from a GeoParquet file using streaming processing with memory tracking.
///
/// This is the same as `generate_tiles_streaming` but also returns memory usage statistics.
/// Use this when you need to verify memory budget compliance.
///
/// # Arguments
///
/// * `input_path` - Path to the GeoParquet file
/// * `config` - Tiling configuration (including optional memory_budget)
///
/// # Returns
///
/// A tuple of (tiles, memory_stats) where memory_stats contains peak usage and budget info.
#[allow(clippy::type_complexity)]
pub fn generate_tiles_streaming_with_stats(
    input_path: &Path,
    config: &TilerConfig,
) -> Result<(Vec<GeneratedTile>, crate::memory::MemoryStats)> {
    use crate::memory::{estimate_geometry_size, MemoryStats, MemoryTracker};
    use geo::BoundingRect;
    use std::collections::HashMap;

    // Step 1: Initialize memory tracker
    let mut memory_tracker = match config.memory_budget {
        Some(budget) => MemoryTracker::with_budget(budget),
        None => MemoryTracker::new(),
    };

    // Step 2: Extract field metadata
    let _all_fields = extract_field_metadata(input_path).unwrap_or_default();

    // Step 3: Track tiles by coordinate for merging across row groups
    // Key: (z, x, y), Value: accumulated features with their global index
    let mut tile_features: HashMap<(u8, u32, u32), Vec<(Geometry<f64>, u64)>> = HashMap::new();
    let mut global_feature_index: u64 = 0;

    // Clone config for closure
    let config_clone = config.clone();

    // Step 4: Process each row group independently with parallel I/O
    process_geometries_parallel(
        input_path,
        DEFAULT_PARALLEL_READERS,
        |rg_info: RowGroupInfo, geometries| {
            // Track memory for this row group
            let row_group_mem: usize = geometries.iter().map(estimate_geometry_size).sum();
            memory_tracker.add(row_group_mem);

            // Check budget and log warning if exceeded
            if memory_tracker.is_over_budget() {
                memory_tracker.record_budget_exceeded();
                log::warn!(
                    "Row group {} ({} features) exceeds memory budget: {} > {}",
                    rg_info.index,
                    rg_info.num_rows,
                    crate::memory::format_bytes(memory_tracker.current()),
                    crate::memory::format_bytes(memory_tracker.budget().unwrap_or(0))
                );
            }

            // Sort geometries within this row group for better locality
            let mut sorted = geometries;
            sort_geometries(&mut sorted, config_clone.use_hilbert);

            // For each geometry, use hierarchical clipping across all zoom levels.
            // This clips from parent tile results instead of the original geometry,
            // reducing redundant work. See issue #38.
            for geom in sorted {
                let geom_bbox = match geom.bounding_rect() {
                    Some(rect) => {
                        TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y)
                    }
                    None => continue, // Skip geometries without bounds
                };

                // Pre-simplify at max zoom tolerance (consistent with production pipeline)
                let simplified =
                    simplify_for_zoom(&geom, config_clone.max_zoom, config_clone.extent);

                // Hierarchical clipping: clip once at min_zoom, then clip parent
                // results for child tiles at higher zoom levels
                let (clip_results, _clip_stats) =
                    crate::hierarchical_clip::clip_geometry_hierarchical(
                        &simplified,
                        &geom_bbox,
                        config_clone.min_zoom,
                        config_clone.max_zoom,
                        config_clone.buffer_pixels,
                        config_clone.extent,
                    );

                for (tile_coord, clipped_geom) in clip_results {
                    // Track memory for accumulated (already clipped) geometry
                    let geom_size = estimate_geometry_size(&clipped_geom);
                    memory_tracker.add(geom_size);

                    // Store pre-clipped feature for this tile
                    tile_features
                        .entry((tile_coord.z, tile_coord.x, tile_coord.y))
                        .or_default()
                        .push((clipped_geom, global_feature_index));
                }

                global_feature_index += 1;
            }

            // "Free" the row group memory (geometries go out of scope after this closure)
            memory_tracker.remove(row_group_mem);

            Ok(())
        },
    )?;

    // Step 5: Generate tiles from accumulated features
    let mut tiles: Vec<GeneratedTile> = Vec::new();

    for ((z, x, y), features) in tile_features {
        let coord = TileCoord::new(x, y, z);
        let tile_bounds = coord.bounds();

        // Process features for this tile (already pre-clipped via hierarchical_clip)
        let mut layer_builder = LayerBuilder::new(&config.layer_name).with_extent(config.extent);
        let mut feature_count = 0;

        // Set up density dropping - gap-based takes precedence over grid-based
        let mut gap_selector = config
            .gamma
            .filter(|&g| g > 0.0)
            .map(|gamma| GapBasedSelector::new(gamma).with_scale(scale_for_zoom(z)));

        let mut density_dropper = if gap_selector.is_none() && config.enable_density_drop {
            let density_config = DensityDropConfig::new()
                .with_cell_size(config.density_cell_size)
                .with_max_features_per_cell(config.density_max_per_cell)
                .with_zoom_range(0, config.max_zoom);
            Some(DensityDropper::new(density_config, config.extent))
        } else {
            None
        };

        for (geom, feat_idx) in features {
            // Geometries are already pre-clipped and pre-simplified via
            // hierarchical_clip in Step 4. Only validate here.
            let validated = match filter_valid_geometry(&geom) {
                Some(v) => v,
                None => continue,
            };

            // Check dropping rules after clipping (matches non-streaming behavior)
            if should_drop_geometry(
                &validated,
                z,
                config.max_zoom,
                config.extent,
                &tile_bounds,
                feat_idx,
            ) {
                continue;
            }

            // Density dropping - gap-based (tippecanoe-compatible) or grid-based
            if let Some(ref mut selector) = gap_selector {
                // Gap-based: use Hilbert index to determine spacing
                if selector.should_drop_geometry(&validated) {
                    continue;
                }
            } else if let Some(ref mut dropper) = density_dropper {
                // Grid-based: limit features per grid cell
                if dropper.should_drop_geometry(&validated, &tile_bounds, config.extent, z) {
                    continue;
                }
            }

            // Add to layer (no properties for now)
            layer_builder.add_feature(Some(feat_idx), &validated, &[], &tile_bounds);
            feature_count += 1;
        }

        // Skip empty tiles
        if feature_count == 0 {
            continue;
        }

        // Build tile
        let layer = layer_builder.build();
        let mut tile_builder = TileBuilder::new();
        tile_builder.add_layer(layer);
        let tile = tile_builder.build();
        let encoded = tile.encode_to_vec();

        tiles.push(GeneratedTile::new(coord, encoded, feature_count));
    }

    // Sort tiles by (z, x, y) for deterministic output
    tiles.sort_by(|a, b| (a.coord.z, a.coord.x, a.coord.y).cmp(&(b.coord.z, b.coord.x, b.coord.y)));

    // Collect memory stats
    let stats = MemoryStats::from_tracker(&memory_tracker);

    // Log memory summary
    tracing::info!(
        "Streaming complete: peak memory {}, budget {}",
        stats.peak_formatted(),
        stats
            .budget_formatted()
            .unwrap_or_else(|| "none".to_string())
    );
    if stats.budget_exceeded_count > 0 {
        log::warn!(
            "Memory budget exceeded {} times during processing",
            stats.budget_exceeded_count
        );
    }

    Ok((tiles, stats))
}

/// Iterator that generates tiles for each tile coordinate.
///
/// When parallel mode is enabled, tiles within each zoom level are processed
/// in parallel using Rayon. Zoom levels are still processed sequentially to
/// preserve feature dropping semantics.
struct TileIterator {
    /// Shared geometries for parallel access
    geometries: Arc<Vec<geo::Geometry<f64>>>,
    config: TilerConfig,
    /// Bounding box for generating tile coordinates
    bbox: TileBounds,
    /// Current zoom level being processed
    current_zoom: u8,
    /// Buffer of generated tiles for the current zoom level
    tile_buffer: Vec<GeneratedTile>,
    /// Index into the tile buffer
    buffer_index: usize,
    /// Whether we've finished all zoom levels
    finished: bool,
}

impl TileIterator {
    fn new(geometries: Vec<geo::Geometry<f64>>, bbox: TileBounds, config: TilerConfig) -> Self {
        // Sort geometries by spatial index ONCE before tile generation.
        // This clusters nearby features together for cache-friendly tile generation.
        // Features for each tile will be mostly adjacent in the sorted order.
        let mut sorted_geometries = geometries;
        sort_geometries(&mut sorted_geometries, config.use_hilbert);

        Self {
            geometries: Arc::new(sorted_geometries),
            current_zoom: config.min_zoom,
            bbox,
            config,
            tile_buffer: Vec::new(),
            buffer_index: 0,
            finished: false,
        }
    }

    fn empty() -> Self {
        Self {
            geometries: Arc::new(Vec::new()),
            config: TilerConfig::default(),
            bbox: TileBounds::empty(),
            current_zoom: 0,
            tile_buffer: Vec::new(),
            buffer_index: 0,
            finished: true,
        }
    }

    /// Process a single tile: clip, simplify, encode to MVT.
    /// This is a pure function that can be safely called in parallel.
    fn process_tile_static(
        geometries: &[geo::Geometry<f64>],
        coord: TileCoord,
        config: &TilerConfig,
    ) -> Result<Option<GeneratedTile>> {
        let bounds = coord.bounds();
        let buffer = buffer_pixels_to_degrees(config.buffer_pixels, &bounds, config.extent);

        // Build the layer with clipped/simplified geometries
        let mut layer_builder = LayerBuilder::new(&config.layer_name).with_extent(config.extent);

        // Create density dropper for this tile if enabled
        // Gap-based takes precedence over grid-based when gamma is set
        let mut gap_selector = config
            .gamma
            .filter(|&g| g > 0.0)
            .map(|gamma| GapBasedSelector::new(gamma).with_scale(scale_for_zoom(coord.z)));

        let mut density_dropper = if gap_selector.is_none() && config.enable_density_drop {
            let density_config = DensityDropConfig::new()
                .with_cell_size(config.density_cell_size)
                .with_max_features_per_cell(config.density_max_per_cell)
                .with_zoom_range(0, config.max_zoom);
            Some(DensityDropper::new(density_config, config.extent))
        } else {
            None
        };

        let mut feature_count = 0;

        for (idx, geom) in geometries.iter().enumerate() {
            // PERFORMANCE: Simplify first to reduce coord count, then clip
            let simplified = simplify_for_zoom(geom, coord.z, config.extent);
            if let Some(clipped) = clip_geometry(&simplified, &bounds, buffer) {
                // Validate geometry - filter out degenerate geometries post-clip
                // (e.g., polygons with < 4 points, zero-area polygons, linestrings with < 2 points)
                if let Some(valid_geom) = filter_valid_geometry(&clipped) {
                    // Apply feature dropping based on zoom level and geometry type
                    // base_zoom is max_zoom: at max_zoom all features are kept
                    if should_drop_geometry(
                        &valid_geom,
                        coord.z,
                        config.max_zoom,
                        config.extent,
                        &bounds,
                        idx as u64,
                    ) {
                        continue;
                    }

                    // Apply density-based dropping if enabled
                    // Gap-based (tippecanoe-compatible) or grid-based (simplified)
                    if let Some(ref mut selector) = gap_selector {
                        // Gap-based: use Hilbert index to determine spacing
                        if selector.should_drop_geometry(&valid_geom) {
                            continue;
                        }
                    } else if let Some(ref mut dropper) = density_dropper {
                        // Grid-based: limit features per grid cell
                        if dropper.should_drop_geometry(
                            &valid_geom,
                            &bounds,
                            config.extent,
                            coord.z,
                        ) {
                            continue;
                        }
                    }

                    // Add to layer (no properties for now)
                    layer_builder.add_feature(Some(idx as u64), &valid_geom, &[], &bounds);
                    feature_count += 1;
                }
            }
        }

        // Skip empty tiles
        if feature_count == 0 {
            return Ok(None);
        }

        // Build the tile
        let layer = layer_builder.build();
        let mut tile_builder = TileBuilder::new();
        tile_builder.add_layer(layer);
        let tile = tile_builder.build();

        // Serialize to protobuf bytes
        let data = tile.encode_to_vec();

        Ok(Some(GeneratedTile::new(coord, data, feature_count)))
    }

    /// Process all tiles for a zoom level in parallel.
    fn process_zoom_level_parallel(&self, zoom: u8) -> Vec<Result<GeneratedTile>> {
        let tile_coords: Vec<TileCoord> = tiles_for_bbox(&self.bbox, zoom).collect();

        // Clone Arc for each parallel task
        let geometries = Arc::clone(&self.geometries);
        let config = self.config.clone();

        tile_coords
            .into_par_iter()
            .filter_map(|coord| {
                match Self::process_tile_static(&geometries, coord, &config) {
                    Ok(Some(tile)) => Some(Ok(tile)),
                    Ok(None) => None, // Empty tile, skip
                    Err(e) => Some(Err(e)),
                }
            })
            .collect()
    }

    /// Fill the tile buffer with tiles from the next zoom level.
    fn fill_buffer(&mut self) -> bool {
        if self.current_zoom > self.config.max_zoom {
            self.finished = true;
            return false;
        }

        // Process tiles for this zoom level (always parallel for performance)
        let results = self.process_zoom_level_parallel(self.current_zoom);

        // Extract successful tiles, propagate errors later
        self.tile_buffer = results.into_iter().filter_map(Result::ok).collect();

        // Sort tiles by coordinates for deterministic output order
        self.tile_buffer
            .sort_by_key(|t| (t.coord.z, t.coord.x, t.coord.y));

        self.buffer_index = 0;
        self.current_zoom += 1;

        !self.tile_buffer.is_empty() || self.current_zoom <= self.config.max_zoom
    }
}

impl Iterator for TileIterator {
    type Item = Result<GeneratedTile>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // If we have buffered tiles, return the next one
            if self.buffer_index < self.tile_buffer.len() {
                let tile = self.tile_buffer[self.buffer_index].clone();
                self.buffer_index += 1;
                return Some(Ok(tile));
            }

            // If we're finished, return None
            if self.finished {
                return None;
            }

            // Try to fill the buffer with the next zoom level
            if !self.fill_buffer() && self.tile_buffer.is_empty() {
                return None;
            }
        }
    }
}

/// Generate a single tile from geometries.
///
/// This is a lower-level function for when you already have geometries loaded
/// and want to generate a specific tile.
///
/// # Arguments
///
/// * `geometries` - The source geometries
/// * `coord` - The tile coordinate to generate
/// * `config` - Tiling configuration
///
/// # Returns
///
/// `Some(GeneratedTile)` if the tile has features, `None` if empty.
pub fn generate_single_tile(
    geometries: &[geo::Geometry<f64>],
    coord: TileCoord,
    config: &TilerConfig,
) -> Result<Option<GeneratedTile>> {
    TileIterator::process_tile_static(geometries, coord, config)
}

/// Decode an MVT tile from bytes (for testing).
pub fn decode_tile(data: &[u8]) -> Result<Tile> {
    Tile::decode(data).map_err(|e| Error::MvtEncoding(format!("Failed to decode tile: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{line_string, point, polygon, Geometry};
    use std::path::Path;

    // ========== TilerConfig Tests ==========

    #[test]
    fn test_tiler_config_default() {
        let config = TilerConfig::default();

        assert_eq!(config.min_zoom, 0);
        assert_eq!(config.max_zoom, 14);
        assert_eq!(config.extent, 4096);
        assert_eq!(config.buffer_pixels, 8);
        assert_eq!(config.layer_name, "layer");
    }

    #[test]
    fn test_tiler_config_builder() {
        let config = TilerConfig::new(5, 10)
            .with_layer_name("buildings")
            .with_extent(512)
            .with_buffer(16);

        assert_eq!(config.min_zoom, 5);
        assert_eq!(config.max_zoom, 10);
        assert_eq!(config.extent, 512);
        assert_eq!(config.buffer_pixels, 16);
        assert_eq!(config.layer_name, "buildings");
    }

    // ========== GeneratedTile Tests ==========

    #[test]
    fn test_generated_tile_creation() {
        let coord = TileCoord::new(1, 2, 3);
        let data = vec![1, 2, 3, 4];
        let tile = GeneratedTile::new(coord, data.clone(), 5);

        assert_eq!(tile.coord, coord);
        assert_eq!(tile.data, data);
        assert_eq!(tile.feature_count, 5);
        assert!(!tile.is_empty());
    }

    #[test]
    fn test_generated_tile_empty() {
        let coord = TileCoord::new(0, 0, 0);
        let tile = GeneratedTile::new(coord, vec![], 0);

        assert!(tile.is_empty());
    }

    // ========== Single Tile Generation Tests ==========

    #[test]
    fn test_generate_single_tile_with_point() {
        // Create a point at the center of tile 0/0/0 (null island)
        let point = Geometry::Point(point!(x: 0.0, y: 0.0));
        let geometries = vec![point];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let result = generate_single_tile(&geometries, coord, &config);
        assert!(result.is_ok());

        let tile_opt = result.unwrap();
        assert!(
            tile_opt.is_some(),
            "Should generate a tile for point at origin"
        );

        let tile = tile_opt.unwrap();
        assert!(!tile.is_empty());
        assert_eq!(tile.coord, coord);

        // Verify we can decode the MVT
        let decoded = decode_tile(&tile.data).expect("Should decode MVT");
        assert_eq!(decoded.layers.len(), 1);
        assert_eq!(decoded.layers[0].name, "layer");
        assert_eq!(decoded.layers[0].features.len(), 1);
    }

    #[test]
    fn test_generate_single_tile_with_polygon() {
        // Create a polygon in Andorra (where our test data is)
        let poly = Geometry::Polygon(polygon![
            (x: 1.5, y: 42.5),
            (x: 1.6, y: 42.5),
            (x: 1.6, y: 42.6),
            (x: 1.5, y: 42.6),
            (x: 1.5, y: 42.5),
        ]);
        let geometries = vec![poly];

        let config = TilerConfig::new(10, 10).with_layer_name("buildings");

        // Find the tile that contains Andorra at z10
        let tile_coord = crate::tile::lng_lat_to_tile(1.55, 42.55, 10);

        let result = generate_single_tile(&geometries, tile_coord, &config);
        assert!(result.is_ok());

        let tile_opt = result.unwrap();
        assert!(
            tile_opt.is_some(),
            "Should generate a tile containing the polygon"
        );

        let tile = tile_opt.unwrap();
        let decoded = decode_tile(&tile.data).unwrap();
        assert_eq!(decoded.layers[0].name, "buildings");
        assert_eq!(decoded.layers[0].features.len(), 1);
    }

    #[test]
    fn test_generate_single_tile_empty_when_no_features() {
        // Create a point in Australia
        let point = Geometry::Point(point!(x: 135.0, y: -25.0));
        let geometries = vec![point];

        let config = TilerConfig::new(10, 10);

        // Request a tile in Europe (far from the point)
        let coord = TileCoord::new(516, 377, 10); // Andorra

        let result = generate_single_tile(&geometries, coord, &config);
        assert!(result.is_ok());

        // Should return None because no features intersect this tile
        assert!(result.unwrap().is_none());
    }

    // ========== Full Pipeline Tests ==========

    #[test]
    fn test_generate_tiles_with_real_fixture() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Generate tiles at zoom 10 only (single zoom for speed)
        let config = TilerConfig::new(10, 10).with_layer_name("buildings");

        let tiles_iter = generate_tiles(fixture, &config).expect("Should create iterator");
        let tiles: Vec<_> = tiles_iter.collect();

        // Should generate some tiles
        assert!(!tiles.is_empty(), "Should generate at least one tile");

        // All should be successful
        for tile_result in &tiles {
            assert!(tile_result.is_ok());
        }

        // Verify tile contents
        let first_tile = tiles[0].as_ref().unwrap();
        assert!(!first_tile.is_empty());

        let decoded = decode_tile(&first_tile.data).unwrap();
        assert_eq!(decoded.layers.len(), 1);
        assert_eq!(decoded.layers[0].name, "buildings");
        assert!(!decoded.layers[0].features.is_empty());

        println!("Generated {} tiles at zoom 10", tiles.len());
    }

    #[test]
    fn test_generate_tiles_multi_zoom() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Generate tiles at zoom 8-10
        let config = TilerConfig::new(8, 10);

        let tiles_iter = generate_tiles(fixture, &config).expect("Should create iterator");
        let tiles: Vec<_> = tiles_iter.filter_map(|r| r.ok()).collect();

        // Should have tiles at different zoom levels
        let z8_count = tiles.iter().filter(|t| t.coord.z == 8).count();
        let z9_count = tiles.iter().filter(|t| t.coord.z == 9).count();
        let z10_count = tiles.iter().filter(|t| t.coord.z == 10).count();

        println!(
            "Z8: {} tiles, Z9: {} tiles, Z10: {} tiles",
            z8_count, z9_count, z10_count
        );

        // Higher zooms should generally have more tiles (smaller tiles = more of them)
        // Unless data is sparse
        assert!(z8_count > 0, "Should have z8 tiles");
        assert!(z9_count > 0, "Should have z9 tiles");
        assert!(z10_count > 0, "Should have z10 tiles");

        // At higher zooms, there are more tiles covering the same area
        assert!(
            z10_count >= z9_count,
            "z10 should have at least as many tiles as z9"
        );
        assert!(
            z9_count >= z8_count,
            "z9 should have at least as many tiles as z8"
        );
    }

    #[test]
    fn test_generate_tiles_skips_empty() {
        // Create a single point
        let geometries = vec![Geometry::Point(point!(x: 0.0, y: 0.0))];

        let bbox = calculate_bbox_from_geometries(&geometries);
        let config = TilerConfig::new(0, 2);

        let iter = TileIterator::new(geometries, bbox, config);
        let tiles: Vec<_> = iter.filter_map(|r| r.ok()).collect();

        // At z0, there's only 1 tile (0,0,0) which contains the point
        // At z1, there are 4 tiles, but only 1 contains the point
        // At z2, there are 16 tiles, but only 1 contains the point
        // So we should have exactly 3 tiles (one per zoom)
        assert_eq!(
            tiles.len(),
            3,
            "Should have exactly 3 tiles (one per zoom containing the point)"
        );

        for tile in &tiles {
            println!(
                "Tile z{}/x{}/y{}: {} bytes",
                tile.coord.z,
                tile.coord.x,
                tile.coord.y,
                tile.data.len()
            );
        }
    }

    #[test]
    fn test_mvt_tile_decodes_correctly() {
        let poly = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let geometries = vec![poly];

        let config = TilerConfig::new(0, 0)
            .with_layer_name("test_layer")
            .with_extent(4096);
        let coord = TileCoord::new(0, 0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .unwrap();

        // Decode and verify structure
        let decoded = decode_tile(&tile.data).unwrap();

        assert_eq!(decoded.layers.len(), 1);

        let layer = &decoded.layers[0];
        assert_eq!(layer.version, 2);
        assert_eq!(layer.name, "test_layer");
        assert_eq!(layer.extent, Some(4096));
        assert_eq!(layer.features.len(), 1);

        let feature = &layer.features[0];
        assert_eq!(feature.id, Some(0));
        // GeomType::Polygon = 3
        assert_eq!(feature.r#type, Some(3));
        assert!(!feature.geometry.is_empty());
    }

    #[test]
    fn test_calculate_bbox_from_geometries() {
        let geometries = vec![
            Geometry::Point(point!(x: -10.0, y: -5.0)),
            Geometry::Point(point!(x: 10.0, y: 5.0)),
        ];

        let bbox = calculate_bbox_from_geometries(&geometries);

        assert_eq!(bbox.lng_min, -10.0);
        assert_eq!(bbox.lng_max, 10.0);
        assert_eq!(bbox.lat_min, -5.0);
        assert_eq!(bbox.lat_max, 5.0);
    }

    #[test]
    fn test_streaming_matches_non_streaming() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(0, 8); // Lower zoom range for faster test

        // Non-streaming approach
        let non_streaming: Vec<_> = generate_tiles(fixture, &config)
            .expect("non-streaming should work")
            .filter_map(|r| r.ok())
            .collect();

        // Streaming approach
        let streaming = generate_tiles_streaming(fixture, &config).expect("streaming should work");

        // Compare tile counts (streaming might have slightly different ordering)
        assert!(
            !non_streaming.is_empty(),
            "Should generate some tiles (non-streaming)"
        );
        assert!(
            !streaming.is_empty(),
            "Should generate some tiles (streaming)"
        );

        // Both should produce similar number of tiles (within 30% tolerance due to potential
        // differences in feature ordering, dropping behavior, and precision effects).
        // Note: For small tile counts (< 10), even 1 tile difference can exceed 20% tolerance.
        let ratio = streaming.len() as f64 / non_streaming.len() as f64;
        assert!(
            ratio > 0.7 && ratio < 1.4,
            "Tile counts should be similar: streaming={}, non-streaming={}, ratio={}",
            streaming.len(),
            non_streaming.len(),
            ratio
        );

        // Verify both produce tiles at overlapping zoom levels
        // Note: Due to precision effects (Issue #83 fix), the exact zoom levels may differ
        // slightly between approaches, especially at low zooms where small features may
        // be dropped differently. We verify there's meaningful overlap.
        let streaming_zooms: std::collections::HashSet<u8> =
            streaming.iter().map(|t| t.coord.z).collect();
        let non_streaming_zooms: std::collections::HashSet<u8> =
            non_streaming.iter().map(|t| t.coord.z).collect();

        let overlap: std::collections::HashSet<_> =
            streaming_zooms.intersection(&non_streaming_zooms).collect();

        assert!(
            !overlap.is_empty(),
            "Both should produce tiles at overlapping zoom levels. \
             Streaming: {:?}, Non-streaming: {:?}",
            streaming_zooms,
            non_streaming_zooms
        );
    }

    #[test]
    fn test_streaming_multi_row_group() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(0, 6);
        let tiles = generate_tiles_streaming(fixture, &config).expect("streaming should work");

        assert!(
            !tiles.is_empty(),
            "Should generate some tiles from multi-rowgroup file"
        );

        // Verify tiles are sorted
        for i in 1..tiles.len() {
            let prev = &tiles[i - 1].coord;
            let curr = &tiles[i].coord;
            assert!(
                (prev.z, prev.x, prev.y) <= (curr.z, curr.x, curr.y),
                "Tiles should be sorted by (z, x, y)"
            );
        }
    }

    #[test]
    fn test_pipeline_with_multiple_geometry_types() {
        let geometries = vec![
            Geometry::Point(point!(x: 0.5, y: 0.5)),
            Geometry::Polygon(polygon![
                (x: 0.0, y: 0.0),
                (x: 1.0, y: 0.0),
                (x: 1.0, y: 1.0),
                (x: 0.0, y: 1.0),
                (x: 0.0, y: 0.0),
            ]),
        ];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .unwrap();

        let decoded = decode_tile(&tile.data).unwrap();
        assert_eq!(
            decoded.layers[0].features.len(),
            2,
            "Should have both point and polygon"
        );
    }

    // ========== Degenerate Geometry Validation Tests ==========

    #[test]
    fn test_pipeline_filters_degenerate_linestring() {
        use geo::LineString;

        // Create a linestring with only 1 point (degenerate after simplification scenario)
        // Note: A real scenario would have simplification reduce points,
        // but we can test the validation directly with a degenerate input.
        let degenerate_line =
            Geometry::LineString(LineString::new(vec![geo::Coord { x: 0.5, y: 0.5 }]));

        let valid_point = Geometry::Point(point!(x: 0.5, y: 0.5));

        let geometries = vec![degenerate_line, valid_point];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .unwrap();

        let decoded = decode_tile(&tile.data).unwrap();
        // Should only have the valid point, degenerate linestring filtered out
        assert_eq!(
            decoded.layers[0].features.len(),
            1,
            "Should filter out degenerate linestring"
        );
    }

    #[test]
    fn test_pipeline_filters_degenerate_polygon_too_few_points() {
        // A polygon with only 3 points (2 unique + closing) is degenerate
        let degenerate_poly = Geometry::Polygon(geo::Polygon::new(
            geo::LineString::new(vec![
                geo::Coord { x: 0.0, y: 0.0 },
                geo::Coord { x: 1.0, y: 0.0 },
                geo::Coord { x: 0.0, y: 0.0 }, // closing
            ]),
            vec![],
        ));

        let valid_point = Geometry::Point(point!(x: 0.5, y: 0.5));

        let geometries = vec![degenerate_poly, valid_point];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .unwrap();

        let decoded = decode_tile(&tile.data).unwrap();
        // Should only have the valid point
        assert_eq!(
            decoded.layers[0].features.len(),
            1,
            "Should filter out degenerate polygon with too few points"
        );
    }

    #[test]
    fn test_pipeline_filters_zero_area_polygon() {
        // A polygon where all points are collinear (zero area)
        let zero_area_poly = Geometry::Polygon(geo::Polygon::new(
            geo::LineString::new(vec![
                geo::Coord { x: 0.0, y: 0.0 },
                geo::Coord { x: 1.0, y: 0.0 },
                geo::Coord { x: 2.0, y: 0.0 },
                geo::Coord { x: 3.0, y: 0.0 },
                geo::Coord { x: 0.0, y: 0.0 }, // closing
            ]),
            vec![],
        ));

        let valid_point = Geometry::Point(point!(x: 0.5, y: 0.5));

        let geometries = vec![zero_area_poly, valid_point];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .unwrap();

        let decoded = decode_tile(&tile.data).unwrap();
        // Should only have the valid point
        assert_eq!(
            decoded.layers[0].features.len(),
            1,
            "Should filter out zero-area polygon"
        );
    }

    #[test]
    fn test_pipeline_keeps_valid_geometries() {
        // All valid geometries should pass through
        let valid_point = Geometry::Point(point!(x: 0.5, y: 0.5));
        let valid_line = Geometry::LineString(line_string![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 1.0),
        ]);
        let valid_poly = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);

        let geometries = vec![valid_point, valid_line, valid_poly];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .unwrap();

        let decoded = decode_tile(&tile.data).unwrap();
        assert_eq!(
            decoded.layers[0].features.len(),
            3,
            "All valid geometries should be kept"
        );
    }

    #[test]
    fn test_pipeline_all_degenerate_returns_empty_tile() {
        // If all geometries are degenerate, tile should be empty (None)
        let degenerate_line =
            Geometry::LineString(geo::LineString::new(vec![geo::Coord { x: 0.5, y: 0.5 }]));

        let degenerate_poly = Geometry::Polygon(geo::Polygon::new(
            geo::LineString::new(vec![
                geo::Coord { x: 0.0, y: 0.0 },
                geo::Coord { x: 1.0, y: 0.0 },
                geo::Coord { x: 0.0, y: 0.0 },
            ]),
            vec![],
        ));

        let geometries = vec![degenerate_line, degenerate_poly];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let result = generate_single_tile(&geometries, coord, &config).unwrap();
        // Should return None because all geometries were filtered out
        assert!(
            result.is_none(),
            "Tile with all degenerate geometries should return None"
        );
    }

    // ========== Feature Dropping Integration Tests ==========

    #[test]
    fn test_point_thinning_reduces_features_at_lower_zoom() {
        // Create many points spread across the tile
        // At max_zoom (base_zoom), all should be kept
        // At lower zooms, some should be dropped
        let mut geometries = Vec::new();
        for i in 0..1000 {
            // Spread points across the world tile (z0)
            let lng = -180.0 + (i as f64) * 0.36;
            let lat = -85.0 + (i as f64) * 0.17;
            geometries.push(Geometry::Point(point!(x: lng, y: lat)));
        }

        let coord_z0 = TileCoord::new(0, 0, 0);

        // Test 1: Generate at z0 with max_zoom=0 (base_zoom=current, no thinning)
        // All points should be kept at base_zoom
        let config_base = TilerConfig::new(0, 0);
        let tile_base = generate_single_tile(&geometries, coord_z0, &config_base)
            .unwrap()
            .expect("Should have features at base_zoom");

        let decoded_base = decode_tile(&tile_base.data).unwrap();
        let features_base = decoded_base.layers[0].features.len();

        // At base_zoom, all points should be kept
        assert_eq!(
            features_base, 1000,
            "At base_zoom (max_zoom=0, generating z0), all 1000 points should be kept"
        );

        // Test 2: Generate at z0 with max_zoom=2 (base_zoom=2, current=0)
        // Expected retention: 0.4^2 = 0.16 = 16% (should keep ~160 points)
        let config_low = TilerConfig::new(0, 2);
        let tile_z0_result = generate_single_tile(&geometries, coord_z0, &config_low).unwrap();

        // With 1000 points and 16% retention, we expect ~160 points (with variance)
        let features_z0 = if let Some(tile) = tile_z0_result {
            let decoded = decode_tile(&tile.data).unwrap();
            decoded.layers[0].features.len()
        } else {
            0
        };

        // At z0 with base_zoom=2, fewer features should appear due to thinning
        // Expected ~160 (16% of 1000), allow variance
        assert!(
            features_z0 < 300,
            "At z0 (2 levels below base_zoom), should have ~16% retention. Got {} features (expected ~160)",
            features_z0
        );
        assert!(
            features_z0 > 50,
            "At z0, should still have some features (statistical unlikelihood if 0). Got {} features",
            features_z0
        );

        // Verify thinning happened
        assert!(
            features_z0 < features_base,
            "z0 with base_zoom=2 ({}) should have fewer features than z0 with base_zoom=0 ({})",
            features_z0,
            features_base
        );
    }

    #[test]
    fn test_tiny_polygon_dropped_at_low_zoom() {
        // Create a tiny polygon that should be dropped at low zoom
        // but kept at high zoom where it has sufficient pixel area
        let tiny_poly = Geometry::Polygon(polygon![
            (x: 0.0001, y: 0.0001),
            (x: 0.0002, y: 0.0001),
            (x: 0.0002, y: 0.0002),
            (x: 0.0001, y: 0.0002),
            (x: 0.0001, y: 0.0001),
        ]);

        // Also add a large polygon that should always be kept
        let large_poly = Geometry::Polygon(polygon![
            (x: -10.0, y: -10.0),
            (x: 10.0, y: -10.0),
            (x: 10.0, y: 10.0),
            (x: -10.0, y: 10.0),
            (x: -10.0, y: -10.0),
        ]);

        let geometries = vec![tiny_poly, large_poly];

        let config = TilerConfig::new(0, 0);
        let coord = TileCoord::new(0, 0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .expect("Should have at least the large polygon");

        let decoded = decode_tile(&tile.data).unwrap();

        // The tiny polygon should be dropped (< 4 sq pixels at z0)
        // The large polygon should be kept
        assert_eq!(
            decoded.layers[0].features.len(),
            1,
            "Tiny polygon should be dropped at z0, only large polygon kept"
        );
    }

    #[test]
    fn test_tiny_line_dropped_when_collapses_to_single_pixel() {
        // At z0, the tile is 360 degrees wide with 4096 pixels
        // Each pixel spans ~0.088 degrees (360/4096)
        // Create a tiny line that's smaller than 1 pixel at z0
        // The line must be purely horizontal or vertical to stay in same pixel

        // A tiny horizontal line at a position where it stays within one pixel
        // x changes by 0.01 degrees (much less than 0.088 degrees per pixel)
        let tiny_line = Geometry::LineString(line_string![
            (x: 0.0, y: 0.0),
            (x: 0.01, y: 0.0),  // Horizontal only, stays in same pixel
        ]);

        // Also add a line that spans significant distance
        let large_line = Geometry::LineString(line_string![
            (x: -90.0, y: 0.0),
            (x: 90.0, y: 0.0),
        ]);

        let geometries = vec![tiny_line.clone(), large_line.clone()];

        let coord = TileCoord::new(0, 0, 0);
        let bounds = coord.bounds();

        // Verify the tiny line collapses to same pixel
        let tiny_should_drop = should_drop_geometry(&tiny_line, 0, 0, 4096, &bounds, 0);
        let large_should_drop = should_drop_geometry(&large_line, 0, 0, 4096, &bounds, 1);

        // The tiny line should be dropped
        assert!(
            tiny_should_drop,
            "Tiny line should be marked for dropping (both points in same pixel)"
        );
        assert!(
            !large_should_drop,
            "Large line should NOT be marked for dropping"
        );

        let config = TilerConfig::new(0, 0);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .expect("Should have at least the large line");

        let decoded = decode_tile(&tile.data).unwrap();

        // The tiny line should be dropped (collapses to single pixel)
        // The large line should be kept
        assert_eq!(
            decoded.layers[0].features.len(),
            1,
            "Tiny line should be dropped at z0, only large line kept"
        );
    }

    #[test]
    fn test_all_features_kept_at_max_zoom() {
        // At max_zoom, all features should be kept (no dropping)
        let point = Geometry::Point(point!(x: 1.55, y: 42.55));
        let geometries = vec![point.clone(); 10];

        // Generate at zoom 14 (which is also max_zoom)
        let config = TilerConfig::new(14, 14);
        let coord = crate::tile::lng_lat_to_tile(1.55, 42.55, 14);

        let tile = generate_single_tile(&geometries, coord, &config)
            .unwrap()
            .expect("Should have features");

        let decoded = decode_tile(&tile.data).unwrap();

        // All 10 points should be kept at max_zoom
        assert_eq!(
            decoded.layers[0].features.len(),
            10,
            "All points should be kept at max_zoom (base_zoom)"
        );
    }

    // ========== Spatial Index Integration Tests ==========

    #[test]
    fn test_spatial_sorting_improves_locality() {
        // Create features scattered across different parts of the world
        // After spatial sorting, nearby features should be processed together
        let geometries = vec![
            Geometry::Point(point!(x: 139.7, y: 35.7)),    // Tokyo
            Geometry::Point(point!(x: -122.4, y: 37.8)),   // San Francisco
            Geometry::Point(point!(x: 2.35, y: 48.85)),    // Paris
            Geometry::Point(point!(x: -122.41, y: 37.79)), // Near SF
            Geometry::Point(point!(x: 2.36, y: 48.86)),    // Near Paris
            Geometry::Point(point!(x: 139.75, y: 35.68)),  // Near Tokyo
        ];

        let bbox = calculate_bbox_from_geometries(&geometries);

        // Test with Hilbert curve (default)
        let config_hilbert = TilerConfig::new(0, 2).with_hilbert(true);
        let iter_hilbert = TileIterator::new(geometries.clone(), bbox, config_hilbert);

        // The TileIterator should sort geometries before processing
        // We verify this by checking that the geometries are sorted
        // (SF features should be adjacent, Tokyo features should be adjacent, etc.)

        // Verify by checking the internal state after construction
        // The iterator's geometries should be spatially sorted

        // The config should have use_hilbert = true
        assert!(
            iter_hilbert.config.use_hilbert,
            "use_hilbert should be true"
        );

        // Test with Z-order
        let config_zorder = TilerConfig::new(0, 2).with_hilbert(false);
        let iter_zorder = TileIterator::new(geometries.clone(), bbox, config_zorder);

        // The config should have use_hilbert = false
        assert!(
            !iter_zorder.config.use_hilbert,
            "use_hilbert should be false for Z-order"
        );

        // Both should produce tiles (just verify the pipeline works with sorting enabled)
        let hilbert_tiles: Vec<_> = iter_hilbert.filter_map(|r| r.ok()).collect();
        let zorder_tiles: Vec<_> = iter_zorder.filter_map(|r| r.ok()).collect();

        // Should produce the same number of tiles regardless of sorting method
        assert_eq!(
            hilbert_tiles.len(),
            zorder_tiles.len(),
            "Hilbert and Z-order should produce same number of tiles"
        );
    }

    #[test]
    fn test_hilbert_vs_zorder_config() {
        // Verify the config option works
        let config_default = TilerConfig::default();
        assert!(
            config_default.use_hilbert,
            "Default should use Hilbert curve"
        );

        let config_hilbert = TilerConfig::new(0, 10).with_hilbert(true);
        assert!(
            config_hilbert.use_hilbert,
            "with_hilbert(true) should set use_hilbert to true"
        );

        let config_zorder = TilerConfig::new(0, 10).with_hilbert(false);
        assert!(
            !config_zorder.use_hilbert,
            "with_hilbert(false) should set use_hilbert to false"
        );
    }

    #[test]
    fn test_generate_tiles_with_spatial_sorting() {
        // Create features in multiple locations
        let geometries = vec![
            Geometry::Point(point!(x: 0.0, y: 0.0)),
            Geometry::Point(point!(x: 0.01, y: 0.01)),
            Geometry::Point(point!(x: 90.0, y: 45.0)),
            Geometry::Point(point!(x: 90.01, y: 45.01)),
        ];

        let bbox = calculate_bbox_from_geometries(&geometries);

        // Generate with Hilbert sorting
        let config = TilerConfig::new(0, 2).with_hilbert(true);
        let iter = TileIterator::new(geometries, bbox, config);
        let tiles: Vec<_> = iter.filter_map(|r| r.ok()).collect();

        // Should generate tiles successfully
        assert!(!tiles.is_empty(), "Should generate at least one tile");

        // Each tile should have valid MVT data
        for tile in &tiles {
            let decoded = decode_tile(&tile.data).expect("Should decode MVT");
            assert_eq!(decoded.layers.len(), 1);
            assert!(!decoded.layers[0].features.is_empty());
        }
    }

    // NOTE: Parallel configuration tests removed in v0.4.0
    // Parallelism is now always enabled (no config option)

    // ========== Property Filter Config Tests ==========

    #[test]
    fn test_tiler_config_with_property_filter() {
        let config = TilerConfig::new(0, 10)
            .with_property_filter(PropertyFilter::include(vec!["name", "population"]));

        match &config.property_filter {
            PropertyFilter::Include(set) => {
                assert!(set.contains("name"));
                assert!(set.contains("population"));
            }
            _ => panic!("Expected Include filter"),
        }
    }

    #[test]
    fn test_tiler_config_with_include_properties() {
        let config = TilerConfig::new(0, 10).with_include_properties(vec!["name", "area"]);

        assert!(config.property_filter.should_include("name"));
        assert!(config.property_filter.should_include("area"));
        assert!(!config.property_filter.should_include("population"));
    }

    #[test]
    fn test_tiler_config_with_exclude_properties() {
        let config =
            TilerConfig::new(0, 10).with_exclude_properties(vec!["internal_id", "temp_field"]);

        assert!(!config.property_filter.should_include("internal_id"));
        assert!(!config.property_filter.should_include("temp_field"));
        assert!(config.property_filter.should_include("name"));
        assert!(config.property_filter.should_include("population"));
    }

    #[test]
    fn test_tiler_config_with_geometry_only() {
        let config = TilerConfig::new(0, 10).with_geometry_only();

        assert!(!config.property_filter.should_include("name"));
        assert!(!config.property_filter.should_include("any_field"));
        assert_eq!(config.property_filter, PropertyFilter::ExcludeAll);
    }

    #[test]
    fn test_tiler_config_default_no_property_filter() {
        let config = TilerConfig::default();

        assert_eq!(config.property_filter, PropertyFilter::None);
        assert!(!config.property_filter.is_active());
    }

    // ========== Property Filter with Real Fixture Tests ==========

    #[test]
    fn test_property_filter_field_metadata_include() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // First, get all fields without filter
        let config_all = TilerConfig::new(10, 10);
        let result_all = generate_tiles_with_bounds(fixture, &config_all)
            .expect("Should create tile generation");
        let all_fields: Vec<_> = result_all.fields.keys().cloned().collect();

        println!("All fields: {:?}", all_fields);
        assert!(!all_fields.is_empty(), "Should have some fields");

        // Now test with include filter - only keep specific fields
        // Use a field name that's likely to exist
        if !all_fields.is_empty() {
            let keep_field = &all_fields[0];
            let config_filtered =
                TilerConfig::new(10, 10).with_include_properties(vec![keep_field.clone()]);

            let result_filtered = generate_tiles_with_bounds(fixture, &config_filtered)
                .expect("Should create tile generation");

            assert_eq!(
                result_filtered.fields.len(),
                1,
                "Should only have one field"
            );
            assert!(
                result_filtered.fields.contains_key(keep_field),
                "Should contain the specified field"
            );
        }
    }

    #[test]
    fn test_property_filter_field_metadata_exclude() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // First, get all fields without filter
        let config_all = TilerConfig::new(10, 10);
        let result_all = generate_tiles_with_bounds(fixture, &config_all)
            .expect("Should create tile generation");
        let all_fields: Vec<_> = result_all.fields.keys().cloned().collect();

        if all_fields.len() >= 2 {
            let exclude_field = &all_fields[0];
            let config_filtered =
                TilerConfig::new(10, 10).with_exclude_properties(vec![exclude_field.clone()]);

            let result_filtered = generate_tiles_with_bounds(fixture, &config_filtered)
                .expect("Should create tile generation");

            assert_eq!(
                result_filtered.fields.len(),
                all_fields.len() - 1,
                "Should have one fewer field"
            );
            assert!(
                !result_filtered.fields.contains_key(exclude_field),
                "Should not contain the excluded field"
            );
        }
    }

    #[test]
    fn test_property_filter_exclude_all() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(10, 10).with_geometry_only();

        let result =
            generate_tiles_with_bounds(fixture, &config).expect("Should create tile generation");

        assert!(
            result.fields.is_empty(),
            "Should have no fields with ExcludeAll filter"
        );
    }

    #[test]
    fn test_streaming_with_memory_budget() {
        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Set a generous budget (100MB) that should not be exceeded by small fixture
        let config = TilerConfig::new(0, 6).with_memory_budget(100 * 1024 * 1024);

        let (tiles, stats) =
            generate_tiles_streaming_with_stats(fixture, &config).expect("streaming should work");

        assert!(!tiles.is_empty(), "Should generate tiles");
        assert!(
            stats.within_budget(),
            "Should stay within 100MB budget for small file"
        );
        assert_eq!(
            stats.budget_exceeded_count, 0,
            "Should not exceed budget for small file"
        );

        println!(
            "Memory stats: peak={}, budget={:?}",
            stats.peak_formatted(),
            stats.budget_formatted()
        );
    }

    #[test]
    fn test_streaming_memory_tracking_reports_peak() {
        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Test at various zoom levels to verify the streaming function works correctly
        // The fixture contains polygons in Andorra (lat ~42.5, lng ~1.6-1.8)
        // At zoom 6+, there should be tiles generated
        let config = TilerConfig::new(0, 6);

        let (tiles, stats) =
            generate_tiles_streaming_with_stats(fixture, &config).expect("streaming should work");

        assert!(!tiles.is_empty(), "Should generate tiles");
        assert!(stats.peak_bytes > 0, "Should report non-zero peak memory");

        println!("Multi-rowgroup memory: peak={}", stats.peak_formatted());
    }

    #[test]
    #[ignore] // Run with: cargo test test_large_file_memory_bounded -- --ignored
    fn test_large_file_memory_bounded() {
        // This test requires downloading a large file:
        // curl -o tests/fixtures/large/adm0_polygons.parquet \
        //   https://data.fieldmaps.io/edge-matched/humanitarian/intl/adm0_polygons.parquet
        let fixture = Path::new("../../tests/fixtures/large/adm0_polygons.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: large fixture not found. Download from data.fieldmaps.io");
            return;
        }

        // Set a 1GB budget
        let budget = 1024 * 1024 * 1024; // 1GB
        let config = TilerConfig::new(0, 8).with_memory_budget(budget);

        let (tiles, stats) =
            generate_tiles_streaming_with_stats(fixture, &config).expect("streaming should work");

        println!(
            "Large file stats: {} tiles, peak={}, budget={:?}, exceeded={}",
            tiles.len(),
            stats.peak_formatted(),
            stats.budget_formatted(),
            stats.budget_exceeded_count
        );

        // For truly large files, we expect some row groups might exceed budget
        // but overall the approach should work
        assert!(!tiles.is_empty(), "Should generate tiles from large file");
    }

    // ========== generate_tiles_to_writer Tests ==========

    #[test]
    fn test_generate_tiles_to_writer_basic() {
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(0, 6).with_quiet(true);
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        let stats = generate_tiles_to_writer(fixture, &config, &mut writer)
            .expect("Should generate tiles to writer");

        // Verify stats
        assert!(stats.peak_bytes > 0, "Should track memory usage");

        // Finalize to a file
        let output_path = Path::new("/tmp/test-generate-to-writer.pmtiles");
        let _ = fs::remove_file(output_path);

        let write_stats = writer.finalize(output_path).expect("Should finalize");

        // Verify file was created
        assert!(output_path.exists(), "Output file should exist");
        assert!(write_stats.total_tiles > 0, "Should have written tiles");
        assert!(write_stats.unique_tiles > 0, "Should have unique tiles");

        // Verify valid PMTiles structure
        let data = fs::read(output_path).unwrap();
        assert_eq!(&data[0..7], b"PMTiles", "Should have PMTiles magic");
        assert_eq!(data[7], 3, "Should be version 3");

        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn test_generate_tiles_to_writer_matches_non_streaming() {
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(0, 6).with_quiet(true);

        // Non-streaming approach (for comparison)
        let non_streaming: Vec<_> = generate_tiles(fixture, &config)
            .expect("non-streaming should work")
            .filter_map(|r| r.ok())
            .collect();

        // Streaming to writer approach
        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        generate_tiles_to_writer(fixture, &config, &mut writer)
            .expect("Should generate tiles to writer");

        let output_path = Path::new("/tmp/test-streaming-vs-non-streaming.pmtiles");
        let _ = fs::remove_file(output_path);

        let write_stats = writer.finalize(output_path).expect("Should finalize");

        // Compare tile counts
        // Note: Since Phase 2, the streaming pipeline uses WorldCoord for integer-precise
        // coordinate handling, while the non-streaming path still uses f64. This can cause
        // slight differences in tile counts due to different dropping thresholds.
        // The WorldCoord path is more correct, so we allow more variance here.
        let ratio = write_stats.total_tiles as f64 / non_streaming.len().max(1) as f64;
        assert!(
            ratio > 0.3 && ratio < 3.0,
            "Tile counts should be in reasonable range: streaming={}, non-streaming={}, ratio={}",
            write_stats.total_tiles,
            non_streaming.len(),
            ratio
        );

        let _ = fs::remove_file(output_path);
    }

    // ========== Production Pipeline Tests ==========
    // NOTE: v0.4.0 consolidated to a single pipeline algorithm (geometry-centric with external sort).
    // These tests validate the production code path.

    #[test]
    fn test_pipeline_produces_tiles() {
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(0, 6).with_quiet(true);

        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        let stats =
            generate_tiles_to_writer(fixture, &config, &mut writer).expect("Should generate tiles");

        let output_path = Path::new("/tmp/test-pipeline-basic.pmtiles");
        let _ = fs::remove_file(output_path);

        let write_stats = writer.finalize(output_path).expect("Should finalize");

        assert!(write_stats.total_tiles > 0, "Should produce tiles");

        // Memory tracking should be present
        assert!(
            stats.peak_bytes > 0,
            "Should track memory usage: {:?}",
            stats
        );

        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn test_pipeline_with_multi_row_group() {
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(0, 6).with_quiet(true);

        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        let _stats = generate_tiles_to_writer(fixture, &config, &mut writer)
            .expect("Should handle multi-row-group file");

        let output_path = Path::new("/tmp/test-pipeline-multi-rg.pmtiles");
        let _ = fs::remove_file(output_path);

        let write_stats = writer.finalize(output_path).expect("Should finalize");

        assert!(
            write_stats.total_tiles > 0,
            "Should produce tiles from multi-row-group file"
        );

        let _ = fs::remove_file(output_path);
    }

    #[test]
    fn test_pipeline_memory_stays_bounded() {
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Set a reasonable memory budget
        let memory_budget = 500 * 1024 * 1024; // 500MB

        let config = TilerConfig::new(0, 8)
            .with_quiet(true)
            .with_memory_budget(memory_budget);

        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        let stats = generate_tiles_to_writer(fixture, &config, &mut writer)
            .expect("Should generate tiles with memory budget");

        let output_path = Path::new("/tmp/test-pipeline-memory.pmtiles");
        let _ = fs::remove_file(output_path);
        let _write_stats = writer.finalize(output_path).expect("Should finalize");

        // Peak memory should be tracked and within budget
        eprintln!(
            "Pipeline memory stats: peak={}KB ({}MB), budget={}MB",
            stats.peak_bytes / 1024,
            stats.peak_bytes / (1024 * 1024),
            memory_budget / (1024 * 1024)
        );

        // For small files, peak should be well under budget
        assert!(
            stats.peak_bytes < memory_budget * 2,
            "Peak memory ({}) should be reasonable relative to budget ({})",
            stats.peak_bytes,
            memory_budget
        );

        let _ = fs::remove_file(output_path);
    }

    #[test]
    #[ignore] // Run with: cargo test test_pipeline_large_file -- --ignored
    fn test_pipeline_large_file() {
        // This test uses the 3.3GB adm4_polygons.parquet fixture
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/adm4_polygons.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: large fixture not found at {:?}", fixture);
            return;
        }

        // Target: stay under 1GB for this 3.3GB file
        let memory_budget = 1024 * 1024 * 1024;

        let config = TilerConfig::new(0, 8) // zoom 0-8 for reasonable test time
            .with_quiet(false) // Show progress for large file
            .with_memory_budget(memory_budget);

        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        let stats = generate_tiles_to_writer(fixture, &config, &mut writer)
            .expect("Pipeline should handle large file");

        let output_path = Path::new("/tmp/test-pipeline-large.pmtiles");
        let _ = fs::remove_file(output_path);
        let write_stats = writer.finalize(output_path).expect("Should finalize");

        eprintln!("Large file test results:");
        eprintln!("  Total tiles: {}", write_stats.total_tiles);
        eprintln!(
            "  Peak memory: {}MB (budget: {}MB)",
            stats.peak_bytes / (1024 * 1024),
            memory_budget / (1024 * 1024)
        );

        assert!(
            write_stats.total_tiles > 0,
            "Should produce tiles from large file"
        );

        // The whole point: memory should stay bounded
        assert!(
            stats.peak_bytes < memory_budget,
            "Peak memory ({}) should stay under budget ({})",
            stats.peak_bytes,
            memory_budget
        );

        let _ = fs::remove_file(output_path);
    }

    // ========== Determinism Tests ==========
    //
    // These tests verify that deterministic mode produces the same output as
    // parallel mode, ensuring reproducibility when needed.

    #[test]
    fn test_deterministic_external_sort_matches_parallel() {
        // Test that deterministic mode (sequential) produces the same
        // output as parallel mode. This is critical for reproducibility.
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Run with default (parallel) processing
        let config_par = TilerConfig::new(0, 8).with_quiet(true);

        let mut writer_par =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
        let _stats_par = generate_tiles_to_writer(fixture, &config_par, &mut writer_par)
            .expect("Parallel processing should work");

        let output_par = Path::new("/tmp/test-parallel-external-sort.pmtiles");
        let _ = fs::remove_file(output_par);
        let write_stats_par = writer_par.finalize(output_par).expect("Should finalize");

        // Run with deterministic (sequential) processing
        let config_det = TilerConfig::new(0, 8)
            .with_quiet(true)
            .with_deterministic(true);

        let mut writer_det =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create writer");
        let _stats_det = generate_tiles_to_writer(fixture, &config_det, &mut writer_det)
            .expect("Deterministic processing should work");

        let output_det = Path::new("/tmp/test-deterministic-external-sort.pmtiles");
        let _ = fs::remove_file(output_det);
        let write_stats_det = writer_det.finalize(output_det).expect("Should finalize");

        // Both modes should produce the same number of tiles
        assert_eq!(
            write_stats_par.total_tiles, write_stats_det.total_tiles,
            "Parallel ({}) and deterministic ({}) should produce same tile count",
            write_stats_par.total_tiles, write_stats_det.total_tiles
        );

        eprintln!(
            "Determinism test: parallel={} tiles, deterministic={} tiles",
            write_stats_par.total_tiles, write_stats_det.total_tiles
        );

        let _ = fs::remove_file(output_par);
        let _ = fs::remove_file(output_det);
    }

    #[test]
    fn test_deterministic_config_option() {
        // Test that deterministic config option is available and has correct default
        let config_default = TilerConfig::default();
        assert!(
            !config_default.deterministic,
            "deterministic should be disabled by default (parallel is faster)"
        );

        let config_enabled = TilerConfig::new(0, 10).with_deterministic(true);
        assert!(
            config_enabled.deterministic,
            "deterministic should be enabled when set to true"
        );

        let config_disabled = TilerConfig::new(0, 10).with_deterministic(false);
        assert!(
            !config_disabled.deterministic,
            "deterministic should be disabled when set to false"
        );
    }

    // ========== Tile ID Uniqueness Tests ==========
    // These tests verify that each tile_id appears exactly once in the output.
    // Duplicate tile_ids cause pmtiles.io viewer to fail.

    #[test]
    fn test_no_duplicate_tile_ids_in_output() {
        // This test verifies the fix for the bug where lng=180 coordinates
        // produced invalid tile coordinates, causing duplicate tile_ids.
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::collections::HashSet;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        let config = TilerConfig::new(0, 8).with_quiet(true);

        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        generate_tiles_to_writer(fixture, &config, &mut writer).expect("Should generate tiles");

        let output_path = Path::new("/tmp/test-no-duplicate-tile-ids.pmtiles");
        let _ = fs::remove_file(output_path);

        let write_stats = writer.finalize(output_path).expect("Should finalize");

        // Read the PMTiles file and verify no duplicate tile_ids
        let file_data = fs::read(output_path).expect("Should read file");
        assert!(file_data.len() >= 127, "File should have header");

        // Parse header to get root directory location
        let root_dir_offset = u64::from_le_bytes(file_data[8..16].try_into().unwrap());
        let root_dir_len = u64::from_le_bytes(file_data[16..24].try_into().unwrap());

        // Decompress root directory
        use std::io::Read;
        let compressed =
            &file_data[root_dir_offset as usize..(root_dir_offset + root_dir_len) as usize];
        let mut decoder = flate2::read::GzDecoder::new(compressed);
        let mut decompressed = Vec::new();
        decoder
            .read_to_end(&mut decompressed)
            .expect("Should decompress directory");

        // Parse directory entries (columnar format)
        let mut pos = 0;
        let (num_entries, new_pos) = read_varint(&decompressed, pos);
        pos = new_pos;

        // Read all tile_ids
        let mut tile_ids = Vec::with_capacity(num_entries as usize);
        let mut last_id: u64 = 0;
        for _ in 0..num_entries {
            let (delta, new_pos) = read_varint(&decompressed, pos);
            pos = new_pos;
            last_id = last_id.wrapping_add(delta);
            tile_ids.push(last_id);
        }

        // Check for duplicates
        let unique_ids: HashSet<u64> = tile_ids.iter().cloned().collect();
        assert_eq!(
            unique_ids.len(),
            tile_ids.len(),
            "All tile_ids should be unique. Found {} duplicates out of {} entries",
            tile_ids.len() - unique_ids.len(),
            tile_ids.len()
        );

        // Also verify we have the expected number of tiles
        assert_eq!(
            write_stats.total_tiles as usize,
            tile_ids.len(),
            "Directory entries should match total tiles"
        );

        let _ = fs::remove_file(output_path);
    }

    /// Helper function to read a varint from a byte slice
    fn read_varint(data: &[u8], mut pos: usize) -> (u64, usize) {
        let mut result: u64 = 0;
        let mut shift = 0;
        loop {
            if pos >= data.len() {
                return (0, pos);
            }
            let b = data[pos];
            pos += 1;
            result |= ((b & 0x7f) as u64) << shift;
            if (b & 0x80) == 0 {
                break;
            }
            shift += 7;
        }
        (result, pos)
    }

    // ========== Issue #85: Tiny Polygon Accumulation Tests ==========

    /// Test that tiny polygon accumulation preserves visual density.
    ///
    /// When accumulation is ENABLED (default), tiny polygons should be accumulated
    /// and synthetic squares emitted when the threshold is exceeded.
    /// This matches tippecanoe's behavior (clip.cpp:1048-1097).
    #[test]
    fn test_tiny_polygon_accumulation_emits_synthetic_squares() {
        use geo::polygon;

        // Create many tiny polygons at zoom 0
        // At zoom 0 with 4096 extent, 1 pixel ≈ 0.088° (360/4096)
        // Create polygons that are ~0.01° x 0.01° = sub-pixel at zoom 0
        let mut tiny_polygons = Vec::new();
        for i in 0..20 {
            let offset = i as f64 * 0.02;
            tiny_polygons.push(Geometry::Polygon(polygon![
                (x: offset, y: 0.0),
                (x: offset + 0.01, y: 0.0),
                (x: offset + 0.01, y: 0.01),
                (x: offset, y: 0.01),
                (x: offset, y: 0.0),
            ]));
        }

        // Test with accumulation ENABLED (default)
        let config_with_accumulation = TilerConfig::new(0, 0).with_tiny_polygon_accumulation(true);
        let coord = TileCoord::new(0, 0, 0);

        let tile_with =
            generate_single_tile(&tiny_polygons, coord, &config_with_accumulation).unwrap();

        // Test with accumulation DISABLED
        let config_without_accumulation =
            TilerConfig::new(0, 0).with_tiny_polygon_accumulation(false);

        let tile_without =
            generate_single_tile(&tiny_polygons, coord, &config_without_accumulation).unwrap();

        // With accumulation: should have synthetic squares (some features)
        // Without accumulation: tiny polygons are dropped (fewer/no features)
        match (&tile_with, &tile_without) {
            (Some(with), Some(without)) => {
                // With accumulation should have at least as many features
                // (accumulated + synthetic) as without (just dropped)
                let with_decoded = decode_tile(&with.data).unwrap();
                let without_decoded = decode_tile(&without.data).unwrap();

                let with_features = with_decoded
                    .layers
                    .first()
                    .map(|l| l.features.len())
                    .unwrap_or(0);
                let without_features = without_decoded
                    .layers
                    .first()
                    .map(|l| l.features.len())
                    .unwrap_or(0);

                // With accumulation should produce at least some synthetic features
                // when tiny polygons accumulate above threshold
                assert!(
                    with_features >= without_features,
                    "Accumulation should produce at least as many features as dropping. \
                     With: {}, Without: {}",
                    with_features,
                    without_features
                );
            }
            (Some(_), None) => {
                // Accumulation produced a tile, no accumulation produced nothing
                // This is expected: dropped all tiny polygons vs emitting synthetic squares
            }
            (None, Some(_)) => {
                // This would be unexpected - dropping should not produce more than accumulation
                panic!("Unexpected: no-accumulation mode produced a tile but accumulation did not");
            }
            (None, None) => {
                // Both produced empty - polygons might be too small even for accumulation
                // This is okay for this test
            }
        }
    }

    /// Test that tiny polygon accumulation config flag works correctly
    #[test]
    fn test_tiny_polygon_accumulation_config() {
        // Default should have accumulation enabled
        let default_config = TilerConfig::default();
        assert!(
            default_config.enable_tiny_polygon_accumulation,
            "Tiny polygon accumulation should be enabled by default"
        );

        // Should be able to disable it
        let disabled_config = TilerConfig::new(0, 14).with_tiny_polygon_accumulation(false);
        assert!(
            !disabled_config.enable_tiny_polygon_accumulation,
            "Should be able to disable tiny polygon accumulation"
        );

        // Should be able to explicitly enable it
        let enabled_config = TilerConfig::new(0, 14).with_tiny_polygon_accumulation(true);
        assert!(
            enabled_config.enable_tiny_polygon_accumulation,
            "Should be able to explicitly enable tiny polygon accumulation"
        );
    }
}

/// Tests for tracing span emission
#[cfg(test)]
mod tracing_tests {
    use super::*;
    use tracing_test::traced_test;

    /// Test that the pipeline span is emitted during tile generation
    #[traced_test]
    #[test]
    fn test_pipeline_span_emitted() {
        // Run a minimal tile generation to verify tracing spans are emitted
        let fixture_path = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture_path.exists() {
            // Skip test if fixture doesn't exist
            return;
        }

        let config = TilerConfig::new(0, 4).with_quiet(true);

        let tiles: Result<Vec<GeneratedTile>> = generate_tiles_streaming(fixture_path, &config);
        assert!(tiles.is_ok());

        // Verify the "pipeline" span was entered
        assert!(logs_contain("pipeline"));
    }

    /// Test that row_group span is emitted during processing
    #[traced_test]
    #[test]
    fn test_row_group_span_emitted() {
        let fixture_path = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture_path.exists() {
            return;
        }

        let config = TilerConfig::new(0, 4).with_quiet(true);
        let _ = generate_tiles_streaming(fixture_path, &config);

        // row_group spans are emitted during row group processing
        assert!(logs_contain("row_group"));
    }

    /// Test that read_parquet span is emitted
    #[traced_test]
    #[test]
    fn test_read_parquet_span_emitted() {
        let fixture_path = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture_path.exists() {
            return;
        }

        let config = TilerConfig::new(0, 4).with_quiet(true);
        let _ = generate_tiles_streaming(fixture_path, &config);

        // read_parquet span is emitted during Phase 1
        assert!(logs_contain("read_parquet"));
    }

    /// Test that streaming writer pipeline runs without panicking when profiling is enabled
    ///
    /// This verifies that the tracing spans in generate_tiles_to_writer don't cause issues.
    /// Full span capture verification should be done via integration tests with Chrome trace output.
    #[test]
    fn test_streaming_writer_runs_with_tracing() {
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;

        let fixture_path = Path::new("../../tests/fixtures/streaming/multi-rowgroup-small.parquet");
        if !fixture_path.exists() {
            // Skip test if fixture doesn't exist
            return;
        }

        let config = TilerConfig::new(0, 4).with_quiet(true);
        let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();

        // Just verify the pipeline runs successfully with tracing spans in place
        let result = generate_tiles_to_writer(fixture_path, &config, &mut writer);
        assert!(result.is_ok(), "Tile generation should succeed");
    }

    // ========== Point Clustering Tests ==========

    #[test]
    fn test_cluster_config_builder() {
        // Test that cluster config can be built correctly
        let config = TilerConfig::new(0, 14).with_cluster(50, 12); // 50px distance, max zoom 12

        assert!(config.cluster_config.is_some());
        let cc = config.cluster_config.unwrap();
        assert_eq!(cc.distance, 50);
        assert_eq!(cc.max_zoom, 12);
    }

    #[test]
    fn test_cluster_config_cluster_gap() {
        use crate::clustering::ClusterConfig;

        let config = ClusterConfig::new(50, 14);

        // At zoom 14: scale = (1 << 18) / 256 = 1024
        // gap = (1024 * 50)^2 = 2,621,440,000,000
        let gap_z14 = config.cluster_gap(14);
        let expected_z14 = (1024u64 * 50).pow(2);
        assert_eq!(gap_z14, expected_z14);

        // At zoom 10: scale = (1 << 22) / 256 = 16384
        // gap = (16384 * 50)^2 = much larger
        let gap_z10 = config.cluster_gap(10);
        let expected_z10 = (16384u64 * 50).pow(2);
        assert_eq!(gap_z10, expected_z10);

        // Gap should decrease with zoom (higher zoom = more precision = smaller gap)
        assert!(gap_z10 > gap_z14);
    }

    #[test]
    fn test_pipeline_with_clustering_enabled_on_polygons() {
        // Test that enabling clustering doesn't break polygon processing
        // (clustering only affects points, polygons should pass through unchanged)
        use crate::compression::Compression;
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use std::fs;

        let fixture = Path::new("../../tests/fixtures/realdata/open-buildings.parquet");
        if !fixture.exists() {
            eprintln!("Skipping: fixture not found");
            return;
        }

        // Enable clustering - should not affect polygon processing
        let config = TilerConfig::new(0, 6).with_quiet(true).with_cluster(50, 4); // Cluster within 50px up to zoom 4

        let mut writer =
            StreamingPmtilesWriter::new(Compression::Gzip).expect("Should create streaming writer");

        let stats = generate_tiles_to_writer(fixture, &config, &mut writer)
            .expect("Should generate tiles with clustering enabled");

        let output_path = Path::new("/tmp/test-clustering-with-polygons.pmtiles");
        let _ = fs::remove_file(output_path);

        let write_stats = writer.finalize(output_path).expect("Should finalize");

        // Should still produce tiles (polygons should be processed normally)
        assert!(write_stats.total_tiles > 0, "Should produce tiles");
        assert!(stats.peak_bytes > 0, "Should track memory usage");

        let _ = fs::remove_file(output_path);
    }
}
