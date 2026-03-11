//! Feature dropping logic for zoom-based filtering.
//!
//! This module implements tippecanoe-compatible feature dropping:
//! - **Tiny polygons**: Diffuse probability dropping for polygons < 4 sq pixels
//! - **Tiny lines**: Drop lines when all vertices collapse to the same tile pixel
//! - **Point thinning**: Drop 1/2.5 of points per zoom level above base zoom
//!
//! # Tippecanoe Behavior
//!
//! ## Tiny Polygons
//!
//! > "Any polygons that are smaller than a minimum area (currently 4 square
//! > subpixels) will have their probability diffused, so that some of them
//! > will be drawn as a square of this minimum size and others will not be
//! > drawn at all, preserving the total area that all of them should have
//! > had together."
//!
//! ## Tiny Lines
//!
//! Tippecanoe drops lines when **all vertices collapse to the same tile pixel**
//! after coordinate quantization. There's no explicit pixel length threshold -
//! it's based on whether the geometry has any visual extent at the target zoom.
//!
//! ## Point Thinning
//!
//! > "drops 1/2.5 of the dots for each zoom level above the point base zoom"
//!
//! # Coordinate System
//!
//! Like `simplify.rs`, all calculations are done in tile-local pixel coordinates
//! to ensure consistent behavior regardless of geographic location (latitude).

use crate::tile::{TileBounds, TileCoord};
use crate::world_coord::WorldCoord;
use geo::{Area, Coord, LineString, MultiLineString, MultiPoint, Point, Polygon};
use std::hash::{Hash, Hasher};

// =============================================================================
// POINT THINNING
// =============================================================================

/// The drop factor per zoom level (tippecanoe uses 2.5, so retention is 1/2.5 = 0.4)
pub const POINT_DROP_FACTOR: f64 = 2.5;

/// Returns true if the point should be DROPPED (not kept) at the given zoom level.
///
/// # Arguments
/// * `_point` - The point geometry (currently unused, but included for future extensions
///   like spatial-aware thinning)
/// * `zoom` - Current zoom level being generated
/// * `base_zoom` - The zoom level where all points are kept (typically max_zoom)
/// * `feature_index` - Unique index of this feature for deterministic selection
///
/// # Returns
/// `true` if the point should be dropped, `false` if it should be kept.
///
/// # Example
/// ```
/// use gpq_tiles_core::feature_drop::should_drop_point;
/// use geo::Point;
///
/// let point = Point::new(0.0, 0.0);
///
/// // At base_zoom (14), all points are kept
/// assert!(!should_drop_point(&point, 14, 14, 0));
///
/// // At lower zooms, some points are dropped
/// // (exact behavior depends on feature_index)
/// ```
pub fn should_drop_point(_point: &Point<f64>, zoom: u8, base_zoom: u8, feature_index: u64) -> bool {
    // At or above base_zoom, keep all points
    if zoom >= base_zoom {
        return false;
    }

    // Calculate retention rate: 0.4^(base_zoom - zoom)
    let zoom_diff = (base_zoom - zoom) as u32;
    let retention_rate = (1.0 / POINT_DROP_FACTOR).powi(zoom_diff as i32);

    // Deterministic pseudo-random selection using a simple hash
    let hash = point_deterministic_hash(feature_index);

    // Convert hash to a value in [0, 1)
    let normalized = (hash as f64) / (u64::MAX as f64);

    // Keep if normalized < retention_rate, drop otherwise
    normalized >= retention_rate
}

/// Deterministic hash function for point feature selection.
///
/// Uses a simple but effective mixing function based on the Murmur3 finalizer.
#[inline]
fn point_deterministic_hash(index: u64) -> u64 {
    let mut x = index;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^= x >> 33;
    x
}

/// Returns true if ALL points in the MultiPoint should be dropped.
///
/// MultiPoints are treated as a single feature - either all points are kept
/// or all are dropped. This maintains the semantic integrity of the feature.
pub fn should_drop_multipoint(
    _multipoint: &MultiPoint<f64>,
    zoom: u8,
    base_zoom: u8,
    feature_index: u64,
) -> bool {
    let dummy_point = Point::new(0.0, 0.0);
    should_drop_point(&dummy_point, zoom, base_zoom, feature_index)
}

/// Calculate the retention rate for a given zoom level.
///
/// This is useful for statistics and logging.
pub fn retention_rate(zoom: u8, base_zoom: u8) -> f64 {
    if zoom >= base_zoom {
        return 1.0;
    }

    let zoom_diff = (base_zoom - zoom) as u32;
    (1.0 / POINT_DROP_FACTOR).powi(zoom_diff as i32)
}

/// Default tiny polygon threshold: 4 square pixels (matches tippecanoe)
pub const DEFAULT_TINY_POLYGON_THRESHOLD: f64 = 4.0;

/// Returns true if the polygon should be DROPPED (not kept).
///
/// Uses tippecanoe's diffuse probability algorithm:
/// - Polygons >= threshold are always kept
/// - Polygons < threshold have a probability of being dropped
/// - Smaller polygons have higher drop probability
/// - Zero-area polygons are always dropped
///
/// # Arguments
///
/// * `polygon` - The polygon to check (in geographic coordinates)
/// * `tile_bounds` - The bounds of the tile (for coordinate transformation)
/// * `extent` - Tile extent (typically 4096)
/// * `threshold_sq_pixels` - Minimum area in square pixels (default: 4.0)
///
/// # Returns
///
/// `true` if the polygon should be dropped, `false` if it should be kept.
///
/// # Determinism
///
/// The function uses a hash of the polygon's coordinates to produce
/// deterministic results: the same polygon at the same zoom level will
/// always produce the same drop decision.
pub fn should_drop_tiny_polygon(
    polygon: &Polygon<f64>,
    tile_bounds: &TileBounds,
    extent: u32,
    threshold_sq_pixels: f64,
) -> bool {
    let area = polygon_area_in_tile_coords(polygon, tile_bounds, extent);

    // Zero or negative area (degenerate polygon) is always dropped
    if area <= 0.0 {
        return true;
    }

    // Polygons at or above threshold are always kept
    if area >= threshold_sq_pixels {
        return false;
    }

    // Diffuse probability: smaller polygons have higher drop probability
    // drop_probability = 1.0 - (area / threshold)
    // When area = 0: drop_probability = 1.0 (always drop)
    // When area = threshold: drop_probability = 0.0 (never drop)
    let keep_probability = area / threshold_sq_pixels;

    // Generate a deterministic "random" value from the geometry hash
    // The hash is normalized to [0, 1) range
    let hash = geometry_hash(polygon);
    let hash_normalized = (hash as f64) / (u64::MAX as f64);

    // Drop if the hash value exceeds the keep probability
    // This ensures smaller polygons (lower keep_probability) are dropped more often
    hash_normalized >= keep_probability
}

/// Calculate the area of a polygon in tile-local pixel coordinates.
///
/// Transforms the polygon from geographic coordinates to tile-local
/// coordinates (0-extent) and calculates the unsigned area.
///
/// # Arguments
///
/// * `polygon` - The polygon in geographic coordinates
/// * `tile_bounds` - The bounds of the tile for coordinate transformation
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
///
/// The absolute area in square pixels.
pub fn polygon_area_in_tile_coords(
    polygon: &Polygon<f64>,
    tile_bounds: &TileBounds,
    extent: u32,
) -> f64 {
    // Transform polygon to tile-local coordinates
    let tile_polygon = polygon_to_tile_coords(polygon, tile_bounds, extent);

    // Calculate signed area and return absolute value
    // geo::Area trait returns signed area (positive for CCW, negative for CW)
    tile_polygon.unsigned_area()
}

/// Transform a geographic coordinate to tile-local pixel coordinates.
///
/// Tile coordinates range from 0 to extent (typically 4096).
/// The tile bounds define the geographic extent being mapped.
///
/// # Issue #83 Fix: Precision Alignment with MVT Encoding
///
/// Coordinates are **rounded** to match the precision used by MVT encoding
/// (see `mvt.rs::geo_to_tile_coords` which uses `.round() as i32`).
///
/// This ensures that `polygon_area_in_tile_coords` calculates area using the
/// same discrete coordinates that will appear in the final MVT output. Without
/// this rounding, a polygon could:
/// 1. Pass the `should_drop_tiny_polygon` check (f64 area > 0)
/// 2. Collapse to zero area when encoded to MVT (all corners round to same pixel)
///
/// This caused blank tiles in issue #83.
///
/// # Divergence from Tippecanoe
///
/// Tippecanoe works in 32-bit integer world coordinates throughout the pipeline,
/// so this precision issue doesn't arise. We work in f64 geographic coordinates
/// and round here (for filtering) and in mvt.rs (for encoding) to ensure consistency.
/// See issue #85 for tracking full tippecanoe parity.
#[inline]
fn geo_to_tile_coords(lng: f64, lat: f64, bounds: &TileBounds, extent: u32) -> (f64, f64) {
    let extent_f = extent as f64;

    // Normalize to 0-1 within tile bounds
    let x_ratio = (lng - bounds.lng_min) / (bounds.lng_max - bounds.lng_min);
    let y_ratio = (lat - bounds.lat_min) / (bounds.lat_max - bounds.lat_min);

    // Scale to extent and flip Y (tile coords have Y increasing downward)
    // IMPORTANT: Round to match MVT encoding precision (mvt.rs uses .round() as i32)
    // This ensures filtering decisions align with actual MVT output coordinates.
    let x = (x_ratio * extent_f).round();
    let y = ((1.0 - y_ratio) * extent_f).round();

    (x, y)
}

/// Transform a LineString from geographic to tile-local coordinates.
fn linestring_to_tile_coords(
    ls: &LineString<f64>,
    bounds: &TileBounds,
    extent: u32,
) -> LineString<f64> {
    let coords: Vec<Coord<f64>> = ls
        .coords()
        .map(|c| {
            let (x, y) = geo_to_tile_coords(c.x, c.y, bounds, extent);
            Coord { x, y }
        })
        .collect();
    LineString::new(coords)
}

/// Transform a Polygon from geographic to tile-local coordinates.
fn polygon_to_tile_coords(poly: &Polygon<f64>, bounds: &TileBounds, extent: u32) -> Polygon<f64> {
    let exterior = linestring_to_tile_coords(poly.exterior(), bounds, extent);
    let interiors: Vec<LineString<f64>> = poly
        .interiors()
        .iter()
        .map(|ring| linestring_to_tile_coords(ring, bounds, extent))
        .collect();
    Polygon::new(exterior, interiors)
}

/// Calculate a deterministic hash for a polygon's geometry.
///
/// Uses a simple hash combining all coordinate values to produce
/// consistent drop decisions for the same polygon across multiple calls.
///
/// The hash is designed to:
/// - Be deterministic: same coordinates → same hash
/// - Spread well across the u64 range for good probability distribution
/// - Be fast to compute
fn geometry_hash(polygon: &Polygon<f64>) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();

    // Hash all exterior ring coordinates
    for coord in polygon.exterior().coords() {
        // Convert f64 to bits for consistent hashing
        coord.x.to_bits().hash(&mut hasher);
        coord.y.to_bits().hash(&mut hasher);
    }

    // Hash interior rings as well
    for interior in polygon.interiors() {
        for coord in interior.coords() {
            coord.x.to_bits().hash(&mut hasher);
            coord.y.to_bits().hash(&mut hasher);
        }
    }

    hasher.finish()
}

// =============================================================================
// LINE DROPPING
// =============================================================================

/// Convert a geographic coordinate to tile-local pixel coordinates (integer).
///
/// Returns (x, y) where both are in the range [0, extent).
/// Coordinates outside the tile bounds may be negative or >= extent.
#[inline]
fn to_tile_pixel(coord: &Coord<f64>, bounds: &TileBounds, extent: u32) -> (i32, i32) {
    let extent_f = extent as f64;

    // Normalize to 0-1 within tile bounds
    let x_ratio = (coord.x - bounds.lng_min) / (bounds.lng_max - bounds.lng_min);
    let y_ratio = (coord.y - bounds.lat_min) / (bounds.lat_max - bounds.lat_min);

    // Scale to extent and flip Y (tile coords have Y increasing downward)
    let x = (x_ratio * extent_f).floor() as i32;
    let y = ((1.0 - y_ratio) * extent_f).floor() as i32;

    (x, y)
}

/// Returns true if the line should be DROPPED (not rendered).
///
/// Tippecanoe drops lines when all vertices collapse to the same tile pixel
/// after coordinate quantization. This happens when a line is too small to
/// have any visual extent at the current zoom level.
///
/// # Arguments
///
/// * `line` - The LineString to check (in geographic coordinates)
/// * `_zoom` - The zoom level (unused - bounds already define the tile)
/// * `extent` - The tile extent (typically 4096)
/// * `tile_bounds` - The geographic bounds of the tile
///
/// # Returns
///
/// `true` if the line should be dropped (all points collapse to same pixel),
/// `false` if the line should be kept (has visual extent).
///
/// # Example
///
/// ```
/// use gpq_tiles_core::feature_drop::should_drop_tiny_line;
/// use gpq_tiles_core::tile::TileBounds;
/// use geo::{LineString, Coord};
///
/// let bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
/// let extent = 4096;
///
/// // A line that spans most of the tile - should NOT be dropped
/// let long_line = LineString::new(vec![
///     Coord { x: 0.1, y: 0.1 },
///     Coord { x: 0.9, y: 0.9 },
/// ]);
/// assert!(!should_drop_tiny_line(&long_line, 0, extent, &bounds));
///
/// // A tiny line (all points collapse to same pixel) - should be dropped
/// let tiny_line = LineString::new(vec![
///     Coord { x: 0.5, y: 0.5 },
///     Coord { x: 0.50012, y: 0.5 },
/// ]);
/// assert!(should_drop_tiny_line(&tiny_line, 0, extent, &bounds));
/// ```
pub fn should_drop_tiny_line(
    line: &LineString<f64>,
    _zoom: u8,
    extent: u32,
    tile_bounds: &TileBounds,
) -> bool {
    // Empty lines should be dropped
    if line.0.is_empty() {
        return true;
    }

    // Single-point "lines" should be dropped (they have no extent)
    if line.0.len() == 1 {
        return true;
    }

    // Convert all points to tile-local pixel coordinates
    let first_pixel = to_tile_pixel(&line.0[0], tile_bounds, extent);

    // Check if all points collapse to the same pixel
    line.0
        .iter()
        .skip(1)
        .all(|coord| to_tile_pixel(coord, tile_bounds, extent) == first_pixel)
}

/// Returns true if any line in the MultiLineString should be kept.
///
/// Filters out lines that collapse to a single pixel. If ALL lines
/// collapse, the entire MultiLineString should be dropped.
///
/// # Returns
///
/// `true` if the entire MultiLineString should be dropped,
/// `false` if at least one line has visual extent.
pub fn should_drop_tiny_multiline(
    mls: &MultiLineString<f64>,
    zoom: u8,
    extent: u32,
    tile_bounds: &TileBounds,
) -> bool {
    // Drop if empty
    if mls.0.is_empty() {
        return true;
    }

    // Drop if ALL component lines are too small
    mls.0
        .iter()
        .all(|line| should_drop_tiny_line(line, zoom, extent, tile_bounds))
}

/// Filter a MultiLineString, keeping only lines with visual extent.
///
/// Returns `None` if all lines should be dropped.
pub fn filter_multiline(
    mls: &MultiLineString<f64>,
    zoom: u8,
    extent: u32,
    tile_bounds: &TileBounds,
) -> Option<MultiLineString<f64>> {
    let kept: Vec<LineString<f64>> = mls
        .0
        .iter()
        .filter(|line| !should_drop_tiny_line(line, zoom, extent, tile_bounds))
        .cloned()
        .collect();

    if kept.is_empty() {
        None
    } else {
        Some(MultiLineString::new(kept))
    }
}

// =============================================================================
// DENSITY-BASED DROPPING
// =============================================================================

/// Configuration for density-based feature dropping.
///
/// This implements a tippecanoe-compatible algorithm for reducing feature density
/// at lower zoom levels. Features are assigned to grid cells, and when a cell
/// contains too many features, excess features are dropped deterministically.
#[derive(Debug, Clone)]
pub struct DensityDropConfig {
    /// Grid cell size in pixels (features within same cell compete)
    /// Default: 16 pixels (256 cells per tile at 4096 extent)
    pub cell_size: u32,

    /// Maximum features allowed per grid cell before dropping starts
    /// Default: 1 (tippecanoe's strict approach - one feature per cell)
    pub max_features_per_cell: usize,

    /// Minimum zoom level at which density dropping is applied
    /// Below this zoom, all features are subject to density dropping
    /// Default: 0 (apply at all zoom levels)
    pub min_zoom: u8,

    /// Maximum zoom level (base_zoom) at which all features are kept
    /// At this zoom and above, no density dropping occurs
    /// Default: 14
    pub max_zoom: u8,
}

impl Default for DensityDropConfig {
    fn default() -> Self {
        Self {
            cell_size: 16,
            max_features_per_cell: 1,
            min_zoom: 0,
            max_zoom: 14,
        }
    }
}

impl DensityDropConfig {
    /// Create a new config with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the grid cell size in pixels.
    pub fn with_cell_size(mut self, cell_size: u32) -> Self {
        self.cell_size = cell_size;
        self
    }

    /// Set the maximum features per cell.
    pub fn with_max_features_per_cell(mut self, max: usize) -> Self {
        self.max_features_per_cell = max;
        self
    }

    /// Set the zoom range.
    pub fn with_zoom_range(mut self, min_zoom: u8, max_zoom: u8) -> Self {
        self.min_zoom = min_zoom;
        self.max_zoom = max_zoom;
        self
    }
}

/// Density-based feature dropper that tracks feature counts per grid cell.
///
/// This struct is used during tile generation to determine which features
/// should be dropped based on spatial density.
///
/// # Algorithm
///
/// 1. The tile is divided into a grid of cells (extent / cell_size)
/// 2. Each feature is assigned to a cell based on its centroid
/// 3. Features are processed in order; the first N features per cell are kept
/// 4. Additional features in the same cell are dropped
///
/// # Tippecanoe Compatibility
///
/// This is a simplified version of tippecanoe's gap-based algorithm.
/// Tippecanoe sorts features by Hilbert curve and computes gaps (distances)
/// between consecutive features. We approximate this with a grid-based approach
/// that achieves similar results without requiring Hilbert curve sorting.
///
/// DIVERGENCE FROM TIPPECANOE: Tippecanoe uses Hilbert curve ordering and
/// gap-based selection. We use grid cells instead. This produces similar
/// but not identical results. Tippecanoe's approach is more sophisticated
/// for preserving spatial distribution, while ours is simpler and faster.
#[derive(Debug)]
pub struct DensityDropper {
    config: DensityDropConfig,
    cell_counts: std::collections::HashMap<(u32, u32), usize>,
    grid_size: u32,
}

impl DensityDropper {
    /// Create a new density dropper with the given configuration.
    pub fn new(config: DensityDropConfig, extent: u32) -> Self {
        let grid_size = extent / config.cell_size;
        Self {
            config,
            cell_counts: std::collections::HashMap::new(),
            grid_size,
        }
    }

    /// Create a new density dropper with default configuration.
    pub fn with_defaults(extent: u32) -> Self {
        Self::new(DensityDropConfig::default(), extent)
    }

    /// Reset the dropper for a new tile.
    pub fn reset(&mut self) {
        self.cell_counts.clear();
    }

    /// Check if a feature at the given tile-local coordinates should be dropped.
    ///
    /// Returns `true` if the feature should be dropped due to density constraints.
    ///
    /// # Arguments
    ///
    /// * `x` - X coordinate in tile-local pixels (0 to extent)
    /// * `y` - Y coordinate in tile-local pixels (0 to extent)
    /// * `zoom` - Current zoom level
    ///
    /// # Returns
    ///
    /// `true` if the feature should be dropped, `false` if it should be kept.
    pub fn should_drop(&mut self, x: f64, y: f64, zoom: u8) -> bool {
        // At max_zoom or above, never drop due to density
        if zoom >= self.config.max_zoom {
            return false;
        }

        // Calculate grid cell
        let cell_x = ((x.max(0.0) as u32) / self.config.cell_size).min(self.grid_size - 1);
        let cell_y = ((y.max(0.0) as u32) / self.config.cell_size).min(self.grid_size - 1);
        let cell_key = (cell_x, cell_y);

        // Get current count for this cell
        let count = self.cell_counts.entry(cell_key).or_insert(0);

        // Check if we're over the limit
        if *count >= self.config.max_features_per_cell {
            return true; // Drop this feature
        }

        // Keep this feature, increment count
        *count += 1;
        false
    }

    /// Check if a feature should be dropped based on geometry centroid.
    ///
    /// This is a convenience method that calculates the centroid of a geometry
    /// and checks density at that location.
    pub fn should_drop_geometry(
        &mut self,
        geom: &geo::Geometry<f64>,
        tile_bounds: &TileBounds,
        extent: u32,
        zoom: u8,
    ) -> bool {
        use geo::Centroid;

        // Get centroid of geometry
        let centroid = match geom.centroid() {
            Some(c) => c,
            None => return false, // Can't compute centroid, don't drop
        };

        // Convert to tile-local coordinates
        let (x, y) = geo_to_tile_coords(centroid.x(), centroid.y(), tile_bounds, extent);

        self.should_drop(x, y, zoom)
    }
}

/// Calculate the effective drop rate for a given zoom level.
///
/// This is useful for statistics and logging. The drop rate increases
/// as zoom decreases (more aggressive dropping at lower zooms).
///
/// The formula is based on tippecanoe's behavior where feature density
/// roughly scales with 4^(max_zoom - zoom) but is constrained by the
/// grid-based limiting.
pub fn density_drop_rate(zoom: u8, max_zoom: u8, features_per_cell: usize) -> f64 {
    if zoom >= max_zoom {
        return 0.0; // No dropping at max_zoom
    }

    // At each zoom level below max_zoom, tile area quadruples
    // If we limit to N features per cell, expected drop rate is:
    // 1 - (N / expected_features_per_cell)
    //
    // This is a rough estimate; actual drop rate depends on feature distribution
    let zoom_diff = (max_zoom - zoom) as f64;
    let density_factor = 4.0_f64.powf(zoom_diff);
    let expected_per_cell = density_factor * (features_per_cell as f64);
    let keep_rate = (features_per_cell as f64) / expected_per_cell;
    1.0 - keep_rate
}

// =============================================================================
// WORLDCOORD-NATIVE FEATURE DROPPING (Issue #85 - Integer Coordinate Migration)
// =============================================================================

/// Calculate the area of a ring using the shoelace formula with i64 arithmetic.
///
/// Returns the **signed** area in world coordinate units squared. Positive for
/// counter-clockwise (exterior) rings, negative for clockwise (interior) rings.
///
/// # Algorithm
///
/// Uses the shoelace formula: `2A = Σ(x_i * y_{i+1} - x_{i+1} * y_i)`
///
/// We use i64 for the accumulator because:
/// - Each `x * y` product can be up to `u32::MAX * u32::MAX ≈ 2^64`
/// - But since we're computing differences, they fit in i64
/// - The sum of up to millions of such differences still fits in i64
///
/// # Arguments
/// * `coords` - The ring coordinates (first and last should be the same for closed rings)
///
/// # Returns
/// Signed area * 2 (to avoid division). Divide by 2 for actual area.
pub fn world_ring_area(coords: &[WorldCoord]) -> i64 {
    if coords.len() < 3 {
        return 0;
    }

    let mut sum: i64 = 0;
    for i in 0..coords.len() - 1 {
        let c0 = &coords[i];
        let c1 = &coords[i + 1];
        // Cross product: x0 * y1 - x1 * y0
        // Using i64 to handle the full range of u32 * u32
        sum += (c0.x as i64) * (c1.y as i64) - (c1.x as i64) * (c0.y as i64);
    }

    sum
}

/// Check if a polygon is too small to render at the given tile and zoom.
///
/// This is the WorldCoord-native equivalent of `should_drop_tiny_polygon`.
/// It uses integer arithmetic throughout for precision and performance.
///
/// # Algorithm
///
/// 1. Calculate polygon area in world units using shoelace formula
/// 2. Convert threshold from square pixels to square world units
/// 3. Compare: if area < threshold, the polygon may be dropped
///
/// # Arguments
/// * `exterior` - The exterior ring coordinates
/// * `interiors` - Interior ring coordinates (holes)
/// * `tile` - The tile being rendered
/// * `extent` - Tile extent in pixels (typically 4096)
/// * `threshold_pixels_sq` - Minimum area in square pixels (default: 4.0)
///
/// # Returns
/// `true` if the polygon should be dropped, `false` if it should be kept.
pub fn should_drop_tiny_polygon_world(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    tile: &TileCoord,
    extent: u32,
    threshold_pixels_sq: f64,
) -> bool {
    // Calculate exterior area (absolute value, as winding may vary)
    let exterior_area = world_ring_area(exterior).unsigned_abs();

    // Subtract interior (hole) areas
    let mut total_area = exterior_area;
    for interior in interiors {
        let hole_area = world_ring_area(interior).unsigned_abs();
        total_area = total_area.saturating_sub(hole_area);
    }

    // The shoelace formula returns 2 * area, so divide by 2
    let area_world_units = total_area / 2;

    // Zero area is always dropped
    if area_world_units == 0 {
        return true;
    }

    // Convert threshold from square pixels to square world units
    // At zoom z, one pixel = 2^(32-z) / extent world units
    // So one square pixel = (2^(32-z) / extent)^2 world units squared
    let world_units_per_pixel = if tile.z == 0 {
        (1_u64 << 32) / extent as u64
    } else {
        (1_u64 << (32 - tile.z as u32)) / extent as u64
    };

    // Square pixels → square world units
    // Use u128 for intermediate to avoid overflow
    let world_units_per_pixel_sq =
        (world_units_per_pixel as u128) * (world_units_per_pixel as u128);
    let threshold_world_sq = (threshold_pixels_sq * world_units_per_pixel_sq as f64) as u128;

    // Compare area to threshold
    if (area_world_units as u128) >= threshold_world_sq {
        return false; // Large enough, keep
    }

    // Diffuse probability dropping for small polygons
    let keep_probability = (area_world_units as f64) / (threshold_world_sq as f64);

    // Deterministic hash based on coordinates
    let hash = world_coords_hash(exterior);
    let hash_normalized = (hash as f64) / (u64::MAX as f64);

    hash_normalized >= keep_probability
}

/// Calculate the length of a linestring in world units.
///
/// Uses integer arithmetic for exact computation.
///
/// # Arguments
/// * `coords` - The linestring coordinates
///
/// # Returns
/// The length in world units (not squared).
pub fn world_linestring_length(coords: &[WorldCoord]) -> u64 {
    if coords.len() < 2 {
        return 0;
    }

    let mut total_length: u64 = 0;
    for i in 0..coords.len() - 1 {
        let c0 = &coords[i];
        let c1 = &coords[i + 1];

        // Calculate distance using integer arithmetic
        // dx and dy can be negative, so use i64
        let dx = (c1.x as i64) - (c0.x as i64);
        let dy = (c1.y as i64) - (c0.y as i64);

        // Euclidean distance: sqrt(dx^2 + dy^2)
        // Use f64 for sqrt, convert back to u64
        let dist_sq = (dx * dx + dy * dy) as f64;
        total_length += dist_sq.sqrt() as u64;
    }

    total_length
}

/// Check if a linestring is too short to render at the given tile.
///
/// This is the WorldCoord-native equivalent of `should_drop_tiny_line`.
/// A line is considered "tiny" if its total length is less than the threshold
/// when measured in pixels.
///
/// # Arguments
/// * `coords` - The linestring coordinates
/// * `tile` - The tile being rendered
/// * `extent` - Tile extent in pixels (typically 4096)
/// * `threshold_pixels` - Minimum length in pixels (default: 1.0)
///
/// # Returns
/// `true` if the line should be dropped, `false` if it should be kept.
pub fn should_drop_tiny_line_world(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    threshold_pixels: f64,
) -> bool {
    // Empty or single-point lines should be dropped
    if coords.len() < 2 {
        return true;
    }

    // Calculate length in world units
    let length_world = world_linestring_length(coords);

    // Zero length is always dropped
    if length_world == 0 {
        return true;
    }

    // Convert threshold from pixels to world units
    // At zoom z, one pixel = 2^(32-z) / extent world units
    let world_units_per_pixel = if tile.z == 0 {
        (1_u64 << 32) / extent as u64
    } else {
        (1_u64 << (32 - tile.z as u32)) / extent as u64
    };

    let threshold_world = (threshold_pixels * world_units_per_pixel as f64) as u64;

    // Drop if shorter than threshold
    length_world < threshold_world
}

/// Deterministic hash for WorldCoord arrays.
///
/// Used for consistent probabilistic dropping decisions.
fn world_coords_hash(coords: &[WorldCoord]) -> u64 {
    use std::collections::hash_map::DefaultHasher;

    let mut hasher = DefaultHasher::new();
    for coord in coords {
        coord.x.hash(&mut hasher);
        coord.y.hash(&mut hasher);
    }
    hasher.finish()
}

// =============================================================================
// TinyPolygonAccumulator - Tippecanoe-style tiny polygon accumulation
// =============================================================================
//
// Instead of dropping tiny polygons, this accumulator:
// 1. Tracks accumulated area from tiny polygons
// 2. Tracks the area-weighted centroid of accumulated polygons
// 3. When accumulated area exceeds threshold, emits a synthetic pixel-sized square
//
// This preserves visual density - 10 tiny polygons in a cluster become a single
// visible square, rather than disappearing entirely.
//
// Reference: tippecanoe clip.cpp:1048-1097
// =============================================================================

/// Accumulates tiny polygons and emits synthetic squares when threshold is exceeded.
///
/// Tippecanoe's approach to tiny polygons: instead of dropping them, accumulate
/// their area. When the accumulated area exceeds a threshold (typically 1 pixel²),
/// emit a synthetic pixel-sized square at the centroid of the accumulated polygons.
///
/// This preserves visual density - if an area has many tiny polygons that would
/// individually be too small to see, they collectively produce visible markers.
///
/// # Example
///
/// ```ignore
/// use gpq_tiles_core::feature_drop::{TinyPolygonAccumulator, DEFAULT_TINY_POLYGON_THRESHOLD};
/// use gpq_tiles_core::tile::TileCoord;
/// use gpq_tiles_core::world_coord::WorldCoord;
///
/// let tile = TileCoord::new(8192, 8192, 14);
/// let mut accumulator = TinyPolygonAccumulator::new(tile, 4096, DEFAULT_TINY_POLYGON_THRESHOLD);
///
/// // Add tiny polygons as they're encountered
/// accumulator.accumulate(&exterior_ring, &interior_rings);
///
/// // When threshold is exceeded, emit a synthetic square
/// if accumulator.should_emit() {
///     if let Some((exterior, interiors)) = accumulator.emit_synthetic_square() {
///         // Add synthetic square to tile output
///     }
/// }
/// ```
///
/// # Algorithm (matches tippecanoe clip.cpp:1048-1097)
///
/// 1. For each tiny polygon:
///    - Calculate area in world units
///    - Add to accumulated area
///    - Update weighted centroid
///
/// 2. When accumulated area >= threshold (in world units):
///    - Create a 1-pixel square centered at the weighted centroid
///    - Reset accumulator for next batch
///
/// # Divergence from Tippecanoe
///
/// Tippecanoe accumulates in "long long" (64-bit signed) units and uses
/// world coordinates directly. We use u128 for intermediate calculations
/// to avoid overflow when accumulating many polygons.
#[derive(Debug, Clone)]
pub struct TinyPolygonAccumulator {
    /// The tile this accumulator is collecting for
    tile: TileCoord,
    /// Tile extent in pixels (typically 4096)
    extent: u32,
    /// Threshold in square pixels when to emit synthetic square
    threshold_sq_pixels: f64,
    /// Accumulated area in square world units
    accumulated_area: u128,
    /// Weighted sum of X coordinates (for centroid calculation)
    weighted_x: u128,
    /// Weighted sum of Y coordinates (for centroid calculation)
    weighted_y: u128,
    /// World units per pixel at this zoom level
    world_units_per_pixel: u64,
    /// Threshold in square world units (precomputed from threshold_sq_pixels)
    threshold_world_sq: u128,
}

impl TinyPolygonAccumulator {
    /// Create a new accumulator for the given tile.
    ///
    /// # Arguments
    /// * `tile` - The tile being processed
    /// * `extent` - Tile extent in pixels (typically 4096)
    /// * `threshold_sq_pixels` - Minimum accumulated area in square pixels to emit
    ///
    /// # Returns
    /// A new accumulator with zero accumulated area
    pub fn new(tile: TileCoord, extent: u32, threshold_sq_pixels: f64) -> Self {
        // Calculate world units per pixel at this zoom level
        let world_units_per_pixel = if tile.z >= 32 {
            1_u64
        } else if tile.z == 0 {
            // At zoom 0, tile covers the whole world (2^32 units) with `extent` pixels
            (1_u64 << 32) / extent as u64
        } else {
            // At zoom z, each tile covers 2^(32-z) world units, divided into extent pixels
            (1_u64 << (32 - tile.z as u32)) / extent as u64
        };

        // Convert threshold from square pixels to square world units
        let world_units_per_pixel_sq =
            (world_units_per_pixel as u128) * (world_units_per_pixel as u128);
        let threshold_world_sq = (threshold_sq_pixels * world_units_per_pixel_sq as f64) as u128;

        Self {
            tile,
            extent,
            threshold_sq_pixels,
            accumulated_area: 0,
            weighted_x: 0,
            weighted_y: 0,
            world_units_per_pixel,
            threshold_world_sq,
        }
    }

    /// Accumulate a tiny polygon's area and centroid.
    ///
    /// # Arguments
    /// * `exterior` - The exterior ring coordinates
    /// * `interiors` - Interior ring coordinates (holes)
    ///
    /// The polygon's area is added to the accumulator, and its centroid
    /// contributes to the weighted centroid calculation.
    pub fn accumulate(&mut self, exterior: &[WorldCoord], interiors: &[Vec<WorldCoord>]) {
        // Calculate exterior area using shoelace formula
        let exterior_area = world_ring_area(exterior).unsigned_abs() as u128;

        // Subtract interior (hole) areas
        let interior_area: u128 = interiors
            .iter()
            .map(|ring| world_ring_area(ring).unsigned_abs() as u128)
            .sum();

        // Net area (exterior minus holes)
        let net_area = exterior_area.saturating_sub(interior_area);

        if net_area == 0 {
            return; // Don't accumulate zero-area polygons
        }

        // Calculate centroid of this polygon (simple average of exterior ring)
        // For a more accurate centroid we'd use the signed area formula,
        // but for tiny polygons the difference is negligible
        let (sum_x, sum_y, count) = exterior.iter().fold((0_u128, 0_u128, 0_u128), |acc, c| {
            (acc.0 + c.x as u128, acc.1 + c.y as u128, acc.2 + 1)
        });

        if count == 0 {
            return;
        }

        let centroid_x = sum_x / count;
        let centroid_y = sum_y / count;

        // Update weighted centroid: weight by area
        // weighted_x = sum(area_i * centroid_x_i)
        // weighted_y = sum(area_i * centroid_y_i)
        self.weighted_x += net_area * centroid_x;
        self.weighted_y += net_area * centroid_y;
        self.accumulated_area += net_area;
    }

    /// Get the current accumulated area in square world units.
    ///
    /// Returns 0 if no polygons have been accumulated since the last emission.
    pub fn accumulated_area(&self) -> u128 {
        self.accumulated_area
    }

    /// Check if the accumulated area has exceeded the threshold.
    ///
    /// Returns `true` if a synthetic square should be emitted.
    pub fn should_emit(&self) -> bool {
        self.accumulated_area >= self.threshold_world_sq
    }

    /// Emit a synthetic pixel-sized square at the weighted centroid.
    ///
    /// # Returns
    /// - `Some((exterior, interiors))` if threshold was met, containing the
    ///   synthetic square's exterior ring (5 points, closed) and empty interiors
    /// - `None` if threshold was not met or no area has been accumulated
    ///
    /// # Side Effects
    /// Resets the accumulator after emission (only if threshold was met).
    pub fn emit_synthetic_square(&mut self) -> Option<(Vec<WorldCoord>, Vec<Vec<WorldCoord>>)> {
        // Don't emit if no area accumulated or threshold not met
        if self.accumulated_area == 0 || !self.should_emit() {
            return None;
        }

        // Calculate weighted centroid
        let centroid_x = (self.weighted_x / self.accumulated_area) as u32;
        let centroid_y = (self.weighted_y / self.accumulated_area) as u32;

        // Create a 1-pixel square centered at the centroid
        let half_pixel = (self.world_units_per_pixel / 2) as u32;

        // Handle potential overflow near world coordinate boundaries
        let min_x = centroid_x.saturating_sub(half_pixel);
        let max_x = centroid_x.saturating_add(half_pixel);
        let min_y = centroid_y.saturating_sub(half_pixel);
        let max_y = centroid_y.saturating_add(half_pixel);

        // Create closed ring (5 points)
        let exterior = vec![
            WorldCoord::new(min_x, min_y), // top-left
            WorldCoord::new(max_x, min_y), // top-right
            WorldCoord::new(max_x, max_y), // bottom-right
            WorldCoord::new(min_x, max_y), // bottom-left
            WorldCoord::new(min_x, min_y), // close ring
        ];

        // Reset accumulator for next batch
        self.accumulated_area = 0;
        self.weighted_x = 0;
        self.weighted_y = 0;

        // Return synthetic square with no holes
        Some((exterior, Vec::new()))
    }

    /// Reset the accumulator without emitting.
    ///
    /// Use this when starting a new tile or when you need to discard
    /// accumulated state.
    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.accumulated_area = 0;
        self.weighted_x = 0;
        self.weighted_y = 0;
    }

    /// Get the tile this accumulator is associated with.
    #[allow(dead_code)]
    pub fn tile(&self) -> &TileCoord {
        &self.tile
    }

    /// Get the extent in pixels.
    #[allow(dead_code)]
    pub fn extent(&self) -> u32 {
        self.extent
    }

    /// Get the threshold in square pixels.
    #[allow(dead_code)]
    pub fn threshold_sq_pixels(&self) -> f64 {
        self.threshold_sq_pixels
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Coord, LineString, MultiLineString, Polygon};

    // =========================================================================
    // LINE DROPPING TESTS
    // =========================================================================

    /// Helper to create a TileBounds
    fn bounds(lng_min: f64, lat_min: f64, lng_max: f64, lat_max: f64) -> TileBounds {
        TileBounds::new(lng_min, lat_min, lng_max, lat_max)
    }

    /// Helper to create a LineString from coordinate pairs
    fn line(coords: &[(f64, f64)]) -> LineString<f64> {
        LineString::new(coords.iter().map(|&(x, y)| Coord { x, y }).collect())
    }

    #[test]
    fn test_long_line_never_dropped() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let long_line = line(&[(0.0, 0.0), (1.0, 1.0)]);
        assert!(
            !should_drop_tiny_line(&long_line, 10, extent, &tile_bounds),
            "Long diagonal line should NOT be dropped"
        );
    }

    #[test]
    fn test_horizontal_line_100_pixels() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let horizontal_line = line(&[(0.0, 0.5), (0.0244, 0.5)]);
        assert!(
            !should_drop_tiny_line(&horizontal_line, 10, extent, &tile_bounds),
            "100-pixel horizontal line should NOT be dropped"
        );
    }

    #[test]
    fn test_tiny_line_always_dropped() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let tiny_line = line(&[(0.5, 0.5), (0.50012, 0.5)]);
        assert!(
            should_drop_tiny_line(&tiny_line, 10, extent, &tile_bounds),
            "Tiny line (sub-pixel, horizontal only) should be dropped"
        );
    }

    #[test]
    fn test_zero_length_line_dropped() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let zero_line = line(&[(0.5, 0.5), (0.5, 0.5)]);
        assert!(
            should_drop_tiny_line(&zero_line, 10, extent, &tile_bounds),
            "Zero-length line should be dropped"
        );
    }

    #[test]
    fn test_single_point_line_dropped() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let single_point = line(&[(0.5, 0.5)]);
        assert!(
            should_drop_tiny_line(&single_point, 10, extent, &tile_bounds),
            "Single-point line should be dropped"
        );
    }

    #[test]
    fn test_empty_line_dropped() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let empty_line = LineString::new(vec![]);
        assert!(
            should_drop_tiny_line(&empty_line, 10, extent, &tile_bounds),
            "Empty line should be dropped"
        );
    }

    #[test]
    fn test_multiline_with_one_visible_line_kept() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let mls = MultiLineString::new(vec![
            line(&[(0.0, 0.0), (1.0, 1.0)]),
            line(&[(0.5, 0.5), (0.50001, 0.50001)]),
        ]);
        assert!(
            !should_drop_tiny_multiline(&mls, 10, extent, &tile_bounds),
            "MultiLineString with at least one visible line should NOT be dropped"
        );
    }

    #[test]
    fn test_multiline_all_tiny_dropped() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let mls = MultiLineString::new(vec![
            line(&[(0.5, 0.5), (0.50012, 0.5)]),
            line(&[(0.25, 0.25), (0.25012, 0.25)]),
            line(&[(0.75, 0.75), (0.75012, 0.75)]),
        ]);
        assert!(
            should_drop_tiny_multiline(&mls, 10, extent, &tile_bounds),
            "MultiLineString where ALL lines are tiny should be dropped"
        );
    }

    #[test]
    fn test_filter_multiline_removes_tiny() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let mls = MultiLineString::new(vec![
            line(&[(0.0, 0.0), (1.0, 1.0)]),
            line(&[(0.5, 0.5), (0.5001, 0.5)]),
            line(&[(0.0, 0.5), (0.5, 0.5)]),
        ]);
        let filtered = filter_multiline(&mls, 10, extent, &tile_bounds);
        assert!(filtered.is_some());
        let filtered = filtered.unwrap();
        assert_eq!(filtered.0.len(), 2, "Should have 2 lines after filtering");
    }

    #[test]
    fn test_line_at_tile_edge_short_segment() {
        let tile_bounds = bounds(-180.0, -85.0, 180.0, 85.0);
        let extent = 4096;
        let edge_line = line(&[(179.95, 0.0), (179.951, 0.0)]);
        assert!(
            should_drop_tiny_line(&edge_line, 0, extent, &tile_bounds),
            "Very short line near tile edge should be dropped"
        );
    }

    #[test]
    fn test_line_crosses_multiple_pixels() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let two_pixel_line = line(&[(0.5, 0.5), (0.5005, 0.5)]);
        assert!(
            !should_drop_tiny_line(&two_pixel_line, 10, extent, &tile_bounds),
            "Line spanning 2+ pixels should NOT be dropped"
        );
    }

    #[test]
    fn test_to_tile_pixel_corners() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let bl = to_tile_pixel(&Coord { x: 0.0, y: 0.0 }, &tile_bounds, extent);
        assert_eq!(bl.0, 0);
        assert_eq!(bl.1, 4096);
        let tr = to_tile_pixel(&Coord { x: 1.0, y: 1.0 }, &tile_bounds, extent);
        assert_eq!(tr.0, 4096);
        assert_eq!(tr.1, 0);
    }

    #[test]
    fn test_to_tile_pixel_center() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let center = to_tile_pixel(&Coord { x: 0.5, y: 0.5 }, &tile_bounds, extent);
        assert_eq!(center.0, 2048);
        assert_eq!(center.1, 2048);
    }

    #[test]
    fn test_multipoint_line_all_collapse() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let dense_tiny_line = line(&[
            (0.5, 0.5),
            (0.50004, 0.5),
            (0.50008, 0.5),
            (0.50012, 0.5),
            (0.50016, 0.5),
        ]);
        assert!(
            should_drop_tiny_line(&dense_tiny_line, 10, extent, &tile_bounds),
            "Line with many points that all collapse should be dropped"
        );
    }

    #[test]
    fn test_multipoint_line_one_different() {
        let tile_bounds = bounds(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let mixed_line = line(&[
            (0.5, 0.5),
            (0.50001, 0.50001),
            (0.50002, 0.50002),
            (0.6, 0.6),
            (0.50004, 0.50004),
        ]);
        assert!(
            !should_drop_tiny_line(&mixed_line, 10, extent, &tile_bounds),
            "Line with at least one point in a different pixel should NOT be dropped"
        );
    }

    #[test]
    fn test_line_visible_at_high_zoom_dropped_at_low() {
        let extent = 4096;
        let high_zoom_bounds = bounds(0.0, 0.0, 0.01, 0.01);
        let small_line = line(&[(0.005, 0.005), (0.0051, 0.0051)]);
        assert!(
            !should_drop_tiny_line(&small_line, 14, extent, &high_zoom_bounds),
            "Line should be visible at high zoom (small tile bounds)"
        );
        let low_zoom_bounds = bounds(0.0, 0.0, 10.0, 10.0);
        assert!(
            should_drop_tiny_line(&small_line, 4, extent, &low_zoom_bounds),
            "Line should be dropped at low zoom (large tile bounds)"
        );
    }

    // =========================================================================
    // POLYGON DROPPING TESTS
    // =========================================================================

    /// Create a square polygon centered at the given tile-local coordinates.
    /// `side` is the side length in tile pixels (at extent 4096).
    fn create_square_polygon_in_tile_coords(
        center_x: f64,
        center_y: f64,
        side: f64,
        tile_bounds: &TileBounds,
        extent: u32,
    ) -> Polygon<f64> {
        let extent_f = extent as f64;
        let half = side / 2.0;

        // Define corners in tile coordinates
        let corners_tile = [
            (center_x - half, center_y - half),
            (center_x + half, center_y - half),
            (center_x + half, center_y + half),
            (center_x - half, center_y + half),
            (center_x - half, center_y - half), // Close the ring
        ];

        // Convert to geographic coordinates
        let coords: Vec<Coord<f64>> = corners_tile
            .iter()
            .map(|(x, y)| {
                let x_ratio = x / extent_f;
                let y_ratio = 1.0 - (y / extent_f); // Flip Y
                Coord {
                    x: tile_bounds.lng_min + x_ratio * (tile_bounds.lng_max - tile_bounds.lng_min),
                    y: tile_bounds.lat_min + y_ratio * (tile_bounds.lat_max - tile_bounds.lat_min),
                }
            })
            .collect();

        Polygon::new(LineString::new(coords), vec![])
    }

    // =========================================================================
    // TEST 1: Large polygons are NEVER dropped
    // =========================================================================
    #[test]
    fn test_large_polygon_never_dropped() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Create a 100x100 pixel polygon (10,000 sq pixels >> 4 threshold)
        let polygon =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 100.0, &tile_bounds, extent);

        // Should NEVER be dropped
        let should_drop = should_drop_tiny_polygon(
            &polygon,
            &tile_bounds,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );
        assert!(
            !should_drop,
            "Large polygon (10,000 sq pixels) should never be dropped"
        );
    }

    // =========================================================================
    // TEST 2: Very tiny polygons (< 1 sq pixel) are ALWAYS dropped
    // =========================================================================
    #[test]
    fn test_sub_pixel_polygon_always_dropped() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Create a 0.5x0.5 pixel polygon (0.25 sq pixels << 4 threshold)
        let polygon =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 0.5, &tile_bounds, extent);

        // Verify the area is indeed tiny
        let area = polygon_area_in_tile_coords(&polygon, &tile_bounds, extent);
        assert!(
            area < 1.0,
            "Polygon should be less than 1 sq pixel, got {}",
            area
        );

        // Such tiny polygons should always be dropped (drop_probability ≈ 1.0)
        let should_drop = should_drop_tiny_polygon(
            &polygon,
            &tile_bounds,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );
        assert!(
            should_drop,
            "Sub-pixel polygon (0.25 sq pixels) should always be dropped"
        );
    }

    // =========================================================================
    // TEST 3: Zero-area polygon is ALWAYS dropped
    // =========================================================================
    #[test]
    fn test_zero_area_polygon_always_dropped() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Create a degenerate polygon (all points same location)
        let coords = vec![
            Coord { x: 0.5, y: 0.5 },
            Coord { x: 0.5, y: 0.5 },
            Coord { x: 0.5, y: 0.5 },
            Coord { x: 0.5, y: 0.5 },
        ];
        let polygon = Polygon::new(LineString::new(coords), vec![]);

        let area = polygon_area_in_tile_coords(&polygon, &tile_bounds, extent);
        assert!(
            area.abs() < 1e-10,
            "Degenerate polygon should have zero area"
        );

        let should_drop = should_drop_tiny_polygon(
            &polygon,
            &tile_bounds,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );
        assert!(should_drop, "Zero-area polygon should always be dropped");
    }

    // =========================================================================
    // TEST 4: Polygon exactly at threshold is kept
    // =========================================================================
    #[test]
    fn test_polygon_at_threshold_kept() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Create a 2x2 pixel polygon (4 sq pixels = exactly threshold)
        let polygon =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 2.0, &tile_bounds, extent);

        let area = polygon_area_in_tile_coords(&polygon, &tile_bounds, extent);
        assert!(
            (area - 4.0).abs() < 0.1,
            "Polygon should be ~4 sq pixels, got {}",
            area
        );

        // Polygon at exactly threshold should be kept (not dropped)
        let should_drop = should_drop_tiny_polygon(
            &polygon,
            &tile_bounds,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );
        assert!(
            !should_drop,
            "Polygon exactly at threshold (4 sq pixels) should be kept"
        );
    }

    // =========================================================================
    // TEST 5: Medium polygons (2-3 sq pixels) have probabilistic dropping
    // =========================================================================
    #[test]
    fn test_medium_polygon_diffuse_probability() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Create multiple small polygons at different positions (different hashes)
        // With integer rounding (Issue #83 fix), a 1.414x1.414 f64 polygon rounds
        // to a 2x2 = 4 sq pixel polygon. We use 1.0 size to get ~1 sq pixel after
        // rounding, which is 25% of threshold = ~75% drop probability.
        let mut drop_count = 0;
        let mut keep_count = 0;
        let num_tests = 100;

        for i in 0..num_tests {
            // Create 1.0x1.0 pixel polygon (≈1 sq pixel after rounding)
            // Note: With integer rounding, areas are quantized. A 1.0 pixel square
            // may round to 0, 1, or 2 sq pixels depending on alignment.
            let offset = (i as f64) * 10.0;
            let polygon = create_square_polygon_in_tile_coords(
                1000.0 + offset,
                1000.0 + offset,
                1.0,
                &tile_bounds,
                extent,
            );

            let area = polygon_area_in_tile_coords(&polygon, &tile_bounds, extent);
            // With integer rounding, area should be a small integer (0, 1, 2, or 4)
            assert!(
                area <= 4.0,
                "Polygon {} should be <= 4 sq pixels (small polygon), got {}",
                i,
                area
            );

            if should_drop_tiny_polygon(
                &polygon,
                &tile_bounds,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD,
            ) {
                drop_count += 1;
            } else {
                keep_count += 1;
            }
        }

        // Small polygons (< 4 sq pixel threshold) should be mostly dropped
        // but some may be kept due to probabilistic diffuse dropping
        let drop_ratio = drop_count as f64 / num_tests as f64;
        assert!(
            drop_ratio > 0.5,
            "Expected >50% drop rate for small polygons, got {:.0}% ({} dropped, {} kept)",
            drop_ratio * 100.0,
            drop_count,
            keep_count
        );
    }

    // =========================================================================
    // TEST 6: Deterministic behavior - same polygon always gives same result
    // =========================================================================
    #[test]
    fn test_deterministic_drop_decision() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Create a 1.5x1.5 pixel polygon (~2.25 sq pixels)
        let polygon =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 1.5, &tile_bounds, extent);

        // Call should_drop multiple times
        let first_result = should_drop_tiny_polygon(
            &polygon,
            &tile_bounds,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );

        // All subsequent calls should return the same result
        for _ in 0..100 {
            let result = should_drop_tiny_polygon(
                &polygon,
                &tile_bounds,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD,
            );
            assert_eq!(
                result, first_result,
                "Drop decision should be deterministic for the same polygon"
            );
        }
    }

    // =========================================================================
    // TEST 7: Area calculation is correct for known geometry
    // =========================================================================
    #[test]
    fn test_area_calculation_accuracy() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Test 1: 10x10 pixel square = 100 sq pixels
        let polygon_100 =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 10.0, &tile_bounds, extent);
        let area_100 = polygon_area_in_tile_coords(&polygon_100, &tile_bounds, extent);
        assert!(
            (area_100 - 100.0).abs() < 1.0,
            "10x10 square should be ~100 sq pixels, got {}",
            area_100
        );

        // Test 2: 50x50 pixel square = 2500 sq pixels
        let polygon_2500 =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 50.0, &tile_bounds, extent);
        let area_2500 = polygon_area_in_tile_coords(&polygon_2500, &tile_bounds, extent);
        assert!(
            (area_2500 - 2500.0).abs() < 10.0,
            "50x50 square should be ~2500 sq pixels, got {}",
            area_2500
        );

        // Test 3: 2x2 pixel square = 4 sq pixels (the threshold)
        let polygon_4 =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 2.0, &tile_bounds, extent);
        let area_4 = polygon_area_in_tile_coords(&polygon_4, &tile_bounds, extent);
        assert!(
            (area_4 - 4.0).abs() < 0.1,
            "2x2 square should be ~4 sq pixels, got {}",
            area_4
        );
    }

    // =========================================================================
    // TEST 8: Works at different zoom levels (tile bounds)
    // =========================================================================
    #[test]
    fn test_different_tile_bounds() {
        let extent = 4096;

        // Test with tile at equator (large geographic extent)
        let bounds_equator = TileBounds::new(-10.0, -10.0, 10.0, 10.0);
        let polygon_equator =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 100.0, &bounds_equator, extent);
        assert!(
            !should_drop_tiny_polygon(
                &polygon_equator,
                &bounds_equator,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD
            ),
            "Large polygon should not be dropped regardless of tile bounds"
        );

        // Test with tile at high latitude (smaller geographic extent)
        let bounds_arctic = TileBounds::new(-1.0, 79.0, 1.0, 81.0);
        let polygon_arctic =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 100.0, &bounds_arctic, extent);
        assert!(
            !should_drop_tiny_polygon(
                &polygon_arctic,
                &bounds_arctic,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD
            ),
            "Large polygon should not be dropped regardless of geographic location"
        );
    }

    // =========================================================================
    // TEST 9: Custom threshold works correctly
    // =========================================================================
    #[test]
    fn test_custom_threshold() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;

        // Create a 3x3 pixel polygon (9 sq pixels)
        let polygon =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 3.0, &tile_bounds, extent);

        // With default threshold (4), should NOT be dropped
        assert!(
            !should_drop_tiny_polygon(&polygon, &tile_bounds, extent, 4.0),
            "9 sq pixel polygon should not be dropped with 4 sq pixel threshold"
        );

        // With threshold of 16, should be considered for dropping
        // (9/16 = 0.5625, so ~43% drop probability)
        // We can't assert a specific result, but we can verify it doesn't crash
        let _result = should_drop_tiny_polygon(&polygon, &tile_bounds, extent, 16.0);

        // With threshold of 100, definitely should be dropped eventually
        // (9/100 = 0.09, so ~91% drop probability)
        let polygon_tiny_relative =
            create_square_polygon_in_tile_coords(2048.0, 2048.0, 0.5, &tile_bounds, extent);
        assert!(
            should_drop_tiny_polygon(&polygon_tiny_relative, &tile_bounds, extent, 100.0),
            "0.25 sq pixel polygon should be dropped with 100 sq pixel threshold"
        );
    }

    // =========================================================================
    // TEST 10: Drop probability correlates with size
    // =========================================================================
    #[test]
    fn test_smaller_polygons_dropped_more_often() {
        let tile_bounds = TileBounds::new(0.0, 0.0, 1.0, 1.0);
        let extent = 4096;
        let num_samples = 200;

        // Group 1: ~3 sq pixel polygons (75% of threshold = 25% drop probability)
        let mut drops_3px = 0;
        for i in 0..num_samples {
            let polygon = create_square_polygon_in_tile_coords(
                500.0 + (i as f64) * 5.0,
                500.0,
                1.732, // sqrt(3) ≈ 3 sq pixels
                &tile_bounds,
                extent,
            );
            if should_drop_tiny_polygon(
                &polygon,
                &tile_bounds,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD,
            ) {
                drops_3px += 1;
            }
        }

        // Group 2: ~1 sq pixel polygons (25% of threshold = 75% drop probability)
        let mut drops_1px = 0;
        for i in 0..num_samples {
            let polygon = create_square_polygon_in_tile_coords(
                500.0 + (i as f64) * 5.0,
                2000.0,
                1.0, // 1 sq pixel
                &tile_bounds,
                extent,
            );
            if should_drop_tiny_polygon(
                &polygon,
                &tile_bounds,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD,
            ) {
                drops_1px += 1;
            }
        }

        // Smaller polygons should be dropped more often
        assert!(
            drops_1px > drops_3px,
            "1 sq pixel polygons ({} dropped) should be dropped more often than 3 sq pixel polygons ({} dropped)",
            drops_1px,
            drops_3px
        );
    }

    // =========================================================================
    // POINT THINNING TESTS
    // =========================================================================

    #[test]
    fn test_at_base_zoom_all_points_kept() {
        let point = Point::new(0.0, 0.0);
        let base_zoom = 14;

        for feature_index in 0..1000 {
            assert!(
                !should_drop_point(&point, base_zoom, base_zoom, feature_index),
                "At base_zoom, point with index {} should NOT be dropped",
                feature_index
            );
        }
    }

    #[test]
    fn test_above_base_zoom_all_points_kept() {
        let point = Point::new(0.0, 0.0);
        let base_zoom = 14;

        for feature_index in 0..100 {
            assert!(
                !should_drop_point(&point, 15, base_zoom, feature_index),
                "Above base_zoom, point should NOT be dropped"
            );
            assert!(
                !should_drop_point(&point, 16, base_zoom, feature_index),
                "Above base_zoom, point should NOT be dropped"
            );
        }
    }

    #[test]
    fn test_base_zoom_minus_one_approximately_40_percent_kept() {
        let point = Point::new(0.0, 0.0);
        let base_zoom = 14;
        let zoom = 13;

        let sample_size = 10000;
        let mut kept_count = 0;

        for feature_index in 0..sample_size {
            if !should_drop_point(&point, zoom, base_zoom, feature_index) {
                kept_count += 1;
            }
        }

        let retention = kept_count as f64 / sample_size as f64;
        let expected = 0.4;

        assert!(
            (retention - expected).abs() < 0.05,
            "At base_zoom - 1, expected ~40% retention, got {:.2}% ({} of {})",
            retention * 100.0,
            kept_count,
            sample_size
        );
    }

    #[test]
    fn test_base_zoom_minus_two_approximately_16_percent_kept() {
        let point = Point::new(0.0, 0.0);
        let base_zoom = 14;
        let zoom = 12;

        let sample_size = 10000;
        let mut kept_count = 0;

        for feature_index in 0..sample_size {
            if !should_drop_point(&point, zoom, base_zoom, feature_index) {
                kept_count += 1;
            }
        }

        let retention = kept_count as f64 / sample_size as f64;
        let expected = 0.16;

        assert!(
            (retention - expected).abs() < 0.03,
            "At base_zoom - 2, expected ~16% retention, got {:.2}% ({} of {})",
            retention * 100.0,
            kept_count,
            sample_size
        );
    }

    #[test]
    fn test_point_determinism_same_index_same_result() {
        let point = Point::new(0.0, 0.0);
        let base_zoom = 14;
        let zoom = 10;

        for feature_index in [0, 42, 1000, 999999, u64::MAX / 2] {
            let first_result = should_drop_point(&point, zoom, base_zoom, feature_index);

            for _ in 0..100 {
                let result = should_drop_point(&point, zoom, base_zoom, feature_index);
                assert_eq!(
                    result, first_result,
                    "Determinism violation: feature_index {} gave different results",
                    feature_index
                );
            }
        }
    }

    #[test]
    fn test_different_indices_have_different_outcomes() {
        let point = Point::new(0.0, 0.0);
        let base_zoom = 14;
        let zoom = 12;

        let mut dropped_count = 0;
        let mut kept_count = 0;

        for feature_index in 0..100 {
            if should_drop_point(&point, zoom, base_zoom, feature_index) {
                dropped_count += 1;
            } else {
                kept_count += 1;
            }
        }

        assert!(
            dropped_count > 0,
            "Expected some points to be dropped, but none were"
        );
        assert!(
            kept_count > 0,
            "Expected some points to be kept, but none were"
        );
    }

    #[test]
    fn test_multipoint_uses_same_logic() {
        let point = Point::new(0.0, 0.0);
        let multipoint = MultiPoint::new(vec![
            Point::new(0.0, 0.0),
            Point::new(1.0, 1.0),
            Point::new(2.0, 2.0),
        ]);
        let base_zoom = 14;
        let zoom = 10;

        for feature_index in 0..100 {
            let point_result = should_drop_point(&point, zoom, base_zoom, feature_index);
            let multipoint_result =
                should_drop_multipoint(&multipoint, zoom, base_zoom, feature_index);
            assert_eq!(
                point_result, multipoint_result,
                "MultiPoint and Point should have same drop decision for same index"
            );
        }
    }

    #[test]
    fn test_retention_rate_calculation() {
        let base_zoom = 14;

        assert!((retention_rate(14, base_zoom) - 1.0).abs() < 1e-10);
        assert!((retention_rate(13, base_zoom) - 0.4).abs() < 1e-10);
        assert!((retention_rate(12, base_zoom) - 0.16).abs() < 1e-10);
        assert!((retention_rate(11, base_zoom) - 0.064).abs() < 1e-10);
        assert!((retention_rate(10, base_zoom) - 0.0256).abs() < 1e-10);
    }

    #[test]
    fn test_very_low_zoom_has_very_few_points() {
        let point = Point::new(0.0, 0.0);
        let base_zoom = 14;
        let zoom = 0;

        let sample_size = 100000;
        let mut kept_count = 0;

        for feature_index in 0..sample_size {
            if !should_drop_point(&point, zoom, base_zoom, feature_index) {
                kept_count += 1;
            }
        }

        let retention = kept_count as f64 / sample_size as f64;
        assert!(
            retention < 0.001,
            "At zoom 0, expected <0.1% retention, got {:.4}%",
            retention * 100.0
        );
    }

    #[test]
    fn test_point_deterministic_hash_distribution() {
        let h0 = point_deterministic_hash(0);
        let h1 = point_deterministic_hash(1);
        let h2 = point_deterministic_hash(2);

        assert_ne!(h1.wrapping_sub(h0), 1);
        assert_ne!(h2.wrapping_sub(h1), 1);

        let min = h0.min(h1).min(h2);
        let max = h0.max(h1).max(h2);
        let spread = max - min;

        assert!(
            spread > u64::MAX / 4,
            "Hash distribution seems too narrow: spread = {}",
            spread
        );
    }

    #[test]
    fn test_point_location_does_not_affect_decision() {
        let base_zoom = 14;
        let zoom = 12;
        let feature_index = 42;

        let points = [
            Point::new(0.0, 0.0),
            Point::new(180.0, 90.0),
            Point::new(-180.0, -90.0),
            Point::new(37.7749, -122.4194),
        ];

        let first_result = should_drop_point(&points[0], zoom, base_zoom, feature_index);

        for point in &points[1..] {
            assert_eq!(
                should_drop_point(point, zoom, base_zoom, feature_index),
                first_result,
                "Point location should not affect drop decision (for now)"
            );
        }
    }

    // =========================================================================
    // DENSITY-BASED DROPPING TESTS
    // =========================================================================

    #[test]
    fn test_density_dropper_at_max_zoom_keeps_all() {
        let config = DensityDropConfig::default().with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        // At max_zoom (14), all features should be kept even in same cell
        for i in 0..100 {
            assert!(
                !dropper.should_drop(100.0, 100.0, 14),
                "Feature {} should be kept at max_zoom",
                i
            );
        }
    }

    #[test]
    fn test_density_dropper_first_feature_per_cell_kept() {
        let config = DensityDropConfig::default()
            .with_max_features_per_cell(1)
            .with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        // First feature in a cell should be kept at lower zoom
        assert!(
            !dropper.should_drop(100.0, 100.0, 8),
            "First feature in cell should be kept"
        );
    }

    #[test]
    fn test_density_dropper_second_feature_in_same_cell_dropped() {
        let config = DensityDropConfig::default()
            .with_max_features_per_cell(1)
            .with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        // First feature kept
        let first = dropper.should_drop(100.0, 100.0, 8);
        assert!(!first, "First feature should be kept");

        // Second feature in same cell dropped
        let second = dropper.should_drop(105.0, 105.0, 8); // Same cell (cell_size=16)
        assert!(second, "Second feature in same cell should be dropped");
    }

    #[test]
    fn test_density_dropper_different_cells_both_kept() {
        let config = DensityDropConfig::default()
            .with_max_features_per_cell(1)
            .with_cell_size(16)
            .with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        // Feature in cell (0, 0)
        assert!(
            !dropper.should_drop(8.0, 8.0, 8),
            "First feature in cell (0,0) should be kept"
        );

        // Feature in cell (1, 0) - different cell
        assert!(
            !dropper.should_drop(24.0, 8.0, 8),
            "First feature in cell (1,0) should be kept"
        );

        // Feature in cell (0, 1) - different cell
        assert!(
            !dropper.should_drop(8.0, 24.0, 8),
            "First feature in cell (0,1) should be kept"
        );
    }

    #[test]
    fn test_density_dropper_multiple_features_per_cell() {
        let config = DensityDropConfig::default()
            .with_max_features_per_cell(3)
            .with_cell_size(16)
            .with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        // First 3 features should be kept
        assert!(
            !dropper.should_drop(8.0, 8.0, 8),
            "Feature 1 should be kept"
        );
        assert!(
            !dropper.should_drop(8.0, 8.0, 8),
            "Feature 2 should be kept"
        );
        assert!(
            !dropper.should_drop(8.0, 8.0, 8),
            "Feature 3 should be kept"
        );

        // 4th feature should be dropped
        assert!(
            dropper.should_drop(8.0, 8.0, 8),
            "Feature 4 should be dropped"
        );
    }

    #[test]
    fn test_density_dropper_reset_clears_state() {
        let config = DensityDropConfig::default()
            .with_max_features_per_cell(1)
            .with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        // Fill a cell
        dropper.should_drop(100.0, 100.0, 8);
        assert!(
            dropper.should_drop(100.0, 100.0, 8),
            "Should drop after first"
        );

        // Reset
        dropper.reset();

        // Now should keep again
        assert!(
            !dropper.should_drop(100.0, 100.0, 8),
            "Should keep after reset"
        );
    }

    #[test]
    fn test_density_dropper_high_density_scenario() {
        // Simulate 1000 features clustered in a small area
        let config = DensityDropConfig::default()
            .with_max_features_per_cell(1)
            .with_cell_size(16)
            .with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        let mut kept = 0;
        let mut dropped = 0;

        // All features in same 64x64 pixel area (4x4 = 16 cells with cell_size=16)
        for i in 0..1000 {
            let x = (i % 64) as f64;
            let y = (i / 64 % 64) as f64;
            if dropper.should_drop(x, y, 8) {
                dropped += 1;
            } else {
                kept += 1;
            }
        }

        // With 16 cells (4x4) and 1 feature per cell, we should keep ~16 features
        assert!(
            kept <= 16,
            "Should keep at most 16 features (one per cell), got {}",
            kept
        );
        assert!(
            dropped > 900,
            "Should drop most features when density is high, got {} dropped",
            dropped
        );

        println!(
            "High density test: kept {}, dropped {} (drop rate: {:.1}%)",
            kept,
            dropped,
            (dropped as f64 / 1000.0) * 100.0
        );
    }

    #[test]
    fn test_density_dropper_reduces_feature_count_significantly() {
        // This test simulates a realistic scenario at z8 with ~1000 features
        // We expect density dropping to reduce this significantly
        let config = DensityDropConfig::default()
            .with_max_features_per_cell(1)
            .with_cell_size(32) // Larger cells = more aggressive dropping
            .with_zoom_range(0, 10);
        let mut dropper = DensityDropper::new(config, 4096);

        let mut kept = 0;

        // Distribute 1000 features across a quarter of the tile (2048x2048 area)
        // This simulates a dense area within a tile
        for i in 0..1000 {
            // Spread features across a 512x512 pixel area (roughly 16x16 cells at cell_size=32)
            let x = (i as f64 % 512.0) + 1000.0;
            let y = (i as f64 / 512.0 % 512.0) + 1000.0;

            if !dropper.should_drop(x, y, 8) {
                kept += 1;
            }
        }

        // With 16x16=256 cells max in the 512x512 area (cell_size=32), we expect ~256 kept
        // But features might cluster more, so expect fewer
        println!(
            "Realistic test at z8: {} of 1000 features kept ({:.1}%)",
            kept,
            (kept as f64 / 1000.0) * 100.0
        );

        // Key assertion: significantly fewer than 1000 features
        assert!(
            kept < 500,
            "Density dropping should reduce 1000 features to <500, got {}",
            kept
        );
    }

    #[test]
    fn test_density_drop_config_builder() {
        let config = DensityDropConfig::new()
            .with_cell_size(32)
            .with_max_features_per_cell(2)
            .with_zoom_range(5, 12);

        assert_eq!(config.cell_size, 32);
        assert_eq!(config.max_features_per_cell, 2);
        assert_eq!(config.min_zoom, 5);
        assert_eq!(config.max_zoom, 12);
    }

    #[test]
    fn test_density_drop_rate_calculation() {
        // At max_zoom, no dropping
        assert_eq!(density_drop_rate(14, 14, 1), 0.0);

        // At lower zooms, increasing drop rate
        let rate_z13 = density_drop_rate(13, 14, 1);
        let rate_z12 = density_drop_rate(12, 14, 1);
        let rate_z10 = density_drop_rate(10, 14, 1);

        assert!(rate_z13 > 0.0, "z13 should have some dropping");
        assert!(
            rate_z12 > rate_z13,
            "z12 should have more dropping than z13"
        );
        assert!(
            rate_z10 > rate_z12,
            "z10 should have more dropping than z12"
        );

        // At z10 (4 levels below z14), expect significant dropping
        // Density factor = 4^4 = 256, so expect ~99.6% drop rate
        assert!(
            rate_z10 > 0.99,
            "z10 should have >99% theoretical drop rate, got {}",
            rate_z10
        );
    }

    #[test]
    fn test_density_dropper_edge_coordinates() {
        let config = DensityDropConfig::default().with_zoom_range(0, 14);
        let mut dropper = DensityDropper::new(config, 4096);

        // Test boundary coordinates
        assert!(!dropper.should_drop(0.0, 0.0, 8), "Origin should work");
        dropper.reset();
        assert!(
            !dropper.should_drop(4095.0, 4095.0, 8),
            "Max coords should work"
        );
        dropper.reset();
        assert!(
            !dropper.should_drop(-1.0, -1.0, 8),
            "Negative coords should be clamped"
        );
        dropper.reset();
        assert!(
            !dropper.should_drop(5000.0, 5000.0, 8),
            "Over-max coords should be clamped"
        );
    }

    // =========================================================================
    // TEST: Issue #83 - Precision mismatch between filtering and MVT encoding
    // =========================================================================
    //
    // This test verifies that the area calculation in feature_drop uses the same
    // coordinate precision as MVT encoding. Without this fix, a polygon could:
    // 1. Pass the should_drop_tiny_polygon check (using f64 coords with non-zero area)
    // 2. Collapse to zero area when encoded to MVT (using i32 rounded coords)
    //
    // This caused blank tiles in issue #83.
    #[test]
    fn test_issue_83_precision_mismatch_between_filtering_and_mvt_encoding() {
        // Zoom 0 tile bounds (full world in Web Mercator)
        let tile_bounds = TileBounds::new(-180.0, -85.05112878, 180.0, 85.05112878);
        let extent = 4096u32;

        // At zoom 0:
        // - Tile spans 360° longitude × 170.1° latitude
        // - Each pixel is ~0.088° × 0.042°
        // - Coordinates within ~0.044° will round to the same pixel

        // Create a 0.01° × 0.01° polygon near the origin
        // This is ~0.11 × 0.24 pixels - smaller than 1 pixel
        let small_polygon = Polygon::new(
            LineString::new(vec![
                Coord { x: 0.0, y: 0.0 },
                Coord { x: 0.01, y: 0.0 },
                Coord { x: 0.01, y: 0.01 },
                Coord { x: 0.0, y: 0.01 },
                Coord { x: 0.0, y: 0.0 }, // Close the ring
            ]),
            vec![],
        );

        // Calculate area using our function
        let area = polygon_area_in_tile_coords(&small_polygon, &tile_bounds, extent);

        // The key assertion: at zoom 0, this polygon should have effectively zero area
        // because all its corners round to the same pixel.
        //
        // Before the fix: area would be ~0.027 sq pixels (f64 calculation)
        // After the fix: area should be 0 (i32 rounded calculation)
        assert!(
            area < 0.01,
            "Issue #83: A 0.01° polygon at zoom 0 should have ~0 area after rounding \
             (all corners collapse to same pixel), but got {} sq pixels. \
             This indicates precision mismatch between filtering and MVT encoding.",
            area
        );
    }

    // =========================================================================
    // TEST: Filtering decision must match MVT encoding outcome
    // =========================================================================
    //
    // If should_drop_tiny_polygon returns false (keep the polygon), then the
    // polygon MUST produce valid (non-degenerate) MVT output. This is the
    // invariant that issue #83 violated.
    #[test]
    fn test_filtering_matches_mvt_encoding_precision() {
        // Test at zoom 0 where precision issues are most severe
        let tile_bounds = TileBounds::new(-180.0, -85.05112878, 180.0, 85.05112878);
        let extent = 4096u32;

        // Create polygons of increasing size until one passes the filter
        let sizes = [0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0];

        for size in sizes {
            let polygon = Polygon::new(
                LineString::new(vec![
                    Coord { x: 0.0, y: 0.0 },
                    Coord { x: size, y: 0.0 },
                    Coord { x: size, y: size },
                    Coord { x: 0.0, y: size },
                    Coord { x: 0.0, y: 0.0 },
                ]),
                vec![],
            );

            let should_drop = should_drop_tiny_polygon(
                &polygon,
                &tile_bounds,
                extent,
                DEFAULT_TINY_POLYGON_THRESHOLD,
            );

            if !should_drop {
                // If we decide to KEEP this polygon, verify it has meaningful area
                let area = polygon_area_in_tile_coords(&polygon, &tile_bounds, extent);
                assert!(
                    area >= 1.0,
                    "Issue #83 invariant violated: Polygon of size {}° was kept by filter \
                     but has area {} sq pixels. Kept polygons must have >= 1 sq pixel area \
                     to produce valid MVT output.",
                    size,
                    area
                );

                // Also verify the corners don't all collapse to the same point
                let corners = [
                    geo_to_tile_coords(0.0, 0.0, &tile_bounds, extent),
                    geo_to_tile_coords(size, 0.0, &tile_bounds, extent),
                    geo_to_tile_coords(size, size, &tile_bounds, extent),
                    geo_to_tile_coords(0.0, size, &tile_bounds, extent),
                ];

                // Count unique corners (after rounding, which geo_to_tile_coords should do)
                let unique_corners: std::collections::HashSet<_> = corners
                    .iter()
                    .map(|(x, y)| (x.round() as i32, y.round() as i32))
                    .collect();

                assert!(
                    unique_corners.len() >= 3,
                    "Issue #83 invariant violated: Polygon of size {}° was kept but has only {} \
                     unique corners after rounding. Need at least 3 for valid polygon.",
                    size,
                    unique_corners.len()
                );
            }
        }
    }

    // =========================================================================
    // WORLDCOORD-NATIVE TESTS (Issue #85)
    // =========================================================================

    use crate::world_coord::WorldCoord;

    /// Helper to create a square ring of WorldCoords centered at a position.
    /// Returns coordinates in counter-clockwise order (positive area).
    fn world_square_ring(center_x: u32, center_y: u32, half_side: u32) -> Vec<WorldCoord> {
        vec![
            WorldCoord::new(center_x - half_side, center_y - half_side), // top-left
            WorldCoord::new(center_x - half_side, center_y + half_side), // bottom-left
            WorldCoord::new(center_x + half_side, center_y + half_side), // bottom-right
            WorldCoord::new(center_x + half_side, center_y - half_side), // top-right
            WorldCoord::new(center_x - half_side, center_y - half_side), // close ring
        ]
    }

    // -------------------------------------------------------------------------
    // world_ring_area tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_world_ring_area_unit_square() {
        // A 2x2 square (half_side = 1) should have area 4 world units
        // Shoelace returns 2*area, so we expect 8
        let ring = world_square_ring(100, 100, 1);
        let area = world_ring_area(&ring);
        assert_eq!(
            area.abs(),
            8,
            "2x2 square should have shoelace area of 8 (2*actual area)"
        );
    }

    #[test]
    fn test_world_ring_area_10x10_square() {
        // A 20x20 square (half_side = 10) should have area 400 world units
        // Shoelace returns 2*area, so we expect 800
        let ring = world_square_ring(1000, 1000, 10);
        let area = world_ring_area(&ring);
        assert_eq!(
            area.abs(),
            800,
            "20x20 square should have shoelace area of 800 (2*actual area)"
        );
    }

    #[test]
    fn test_world_ring_area_degenerate() {
        // Degenerate cases
        let empty: Vec<WorldCoord> = vec![];
        assert_eq!(
            world_ring_area(&empty),
            0,
            "Empty ring should have zero area"
        );

        let single = vec![WorldCoord::new(100, 100)];
        assert_eq!(
            world_ring_area(&single),
            0,
            "Single point should have zero area"
        );

        let two_points = vec![WorldCoord::new(100, 100), WorldCoord::new(200, 200)];
        assert_eq!(
            world_ring_area(&two_points),
            0,
            "Two points should have zero area"
        );
    }

    #[test]
    fn test_world_ring_area_large_coordinates() {
        // Test with large coordinates near u32::MAX to verify no overflow
        let center = u32::MAX / 2;
        let half_side = 1_000_000; // Large side, but area still fits in i64
        let ring = world_square_ring(center, center, half_side);
        let area = world_ring_area(&ring);

        // Expected: (2 * half_side)^2 = 4 * 10^12 world units squared
        // Shoelace returns 2x, so 8 * 10^12
        let expected = 2_i64 * (2 * half_side as i64) * (2 * half_side as i64);
        assert_eq!(
            area.abs(),
            expected,
            "Large square area calculation should not overflow"
        );
    }

    // -------------------------------------------------------------------------
    // should_drop_tiny_polygon_world tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_world_polygon_large_never_dropped() {
        // At zoom 14, extent 4096:
        // tile_size = 2^(32-14) = 2^18 = 262144 world units
        // pixel_size = 262144 / 4096 = 64 world units
        // A 1000x1000 world unit square = (1000/64)^2 ≈ 244 sq pixels - definitely kept
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;

        let center_x = 8192_u64 * 262144 + 131072; // Center of tile
        let center_y = 8192_u64 * 262144 + 131072;
        let half_side = 500;

        let exterior = world_square_ring(center_x as u32, center_y as u32, half_side);
        let should_drop = should_drop_tiny_polygon_world(
            &exterior,
            &[],
            &tile,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );

        assert!(
            !should_drop,
            "Large polygon (244 sq pixels) should never be dropped"
        );
    }

    #[test]
    fn test_world_polygon_zero_area_always_dropped() {
        let tile = TileCoord::new(0, 0, 10);
        let extent = 4096;

        // Degenerate polygon (all same point)
        let degenerate = vec![
            WorldCoord::new(1000, 1000),
            WorldCoord::new(1000, 1000),
            WorldCoord::new(1000, 1000),
            WorldCoord::new(1000, 1000),
        ];

        let should_drop = should_drop_tiny_polygon_world(
            &degenerate,
            &[],
            &tile,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );

        assert!(should_drop, "Zero-area polygon should always be dropped");
    }

    #[test]
    fn test_world_polygon_with_hole() {
        // Create a large exterior with a large hole - net area should be small
        let tile = TileCoord::new(0, 0, 10);
        let extent = 4096;

        // Exterior: 1000x1000 = 1,000,000 sq world units
        let exterior = world_square_ring(1_000_000, 1_000_000, 500);

        // Interior (hole): 990x990 ≈ 980,100 sq world units
        // Net area ≈ 19,900 sq world units - still pretty small at this zoom
        let interior = world_square_ring(1_000_000, 1_000_000, 495);

        let should_drop = should_drop_tiny_polygon_world(
            &exterior,
            &[interior],
            &tile,
            extent,
            DEFAULT_TINY_POLYGON_THRESHOLD,
        );

        // At zoom 10, pixel = 2^22 / 4096 = 1024 world units
        // 19,900 sq world units / (1024^2) ≈ 0.019 sq pixels - very tiny
        assert!(
            should_drop,
            "Polygon with large hole (tiny net area) should likely be dropped"
        );
    }

    // -------------------------------------------------------------------------
    // world_linestring_length tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_world_linestring_length_horizontal() {
        let coords = vec![
            WorldCoord::new(100, 100),
            WorldCoord::new(200, 100), // 100 units right
        ];
        let length = world_linestring_length(&coords);
        assert_eq!(length, 100, "Horizontal line should have length 100");
    }

    #[test]
    fn test_world_linestring_length_vertical() {
        let coords = vec![
            WorldCoord::new(100, 100),
            WorldCoord::new(100, 350), // 250 units down
        ];
        let length = world_linestring_length(&coords);
        assert_eq!(length, 250, "Vertical line should have length 250");
    }

    #[test]
    fn test_world_linestring_length_diagonal() {
        // 3-4-5 triangle: length = sqrt(3^2 + 4^2) = 5
        let coords = vec![WorldCoord::new(100, 100), WorldCoord::new(103, 104)];
        let length = world_linestring_length(&coords);
        assert_eq!(length, 5, "3-4-5 diagonal should have length 5");
    }

    #[test]
    fn test_world_linestring_length_multi_segment() {
        // Two segments: 100 horizontal + 100 vertical = 200 total
        let coords = vec![
            WorldCoord::new(0, 0),
            WorldCoord::new(100, 0),   // 100 units
            WorldCoord::new(100, 100), // 100 units
        ];
        let length = world_linestring_length(&coords);
        assert_eq!(length, 200, "Two 100-unit segments should total 200");
    }

    #[test]
    fn test_world_linestring_length_degenerate() {
        let empty: Vec<WorldCoord> = vec![];
        assert_eq!(
            world_linestring_length(&empty),
            0,
            "Empty linestring should have zero length"
        );

        let single = vec![WorldCoord::new(100, 100)];
        assert_eq!(
            world_linestring_length(&single),
            0,
            "Single point should have zero length"
        );

        let zero_length = vec![WorldCoord::new(100, 100), WorldCoord::new(100, 100)];
        assert_eq!(
            world_linestring_length(&zero_length),
            0,
            "Zero-length line should have zero length"
        );
    }

    // -------------------------------------------------------------------------
    // should_drop_tiny_line_world tests
    // -------------------------------------------------------------------------

    #[test]
    fn test_world_line_long_never_dropped() {
        // At zoom 14, pixel = 64 world units
        // A 10000 world unit line = 156 pixels - definitely kept
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;

        let coords = vec![
            WorldCoord::new(2_000_000_000, 2_000_000_000),
            WorldCoord::new(2_000_010_000, 2_000_000_000), // 10000 units horizontal
        ];

        let should_drop = should_drop_tiny_line_world(&coords, &tile, extent, 1.0);
        assert!(
            !should_drop,
            "Long line (156 pixels) should never be dropped"
        );
    }

    #[test]
    fn test_world_line_short_dropped() {
        // At zoom 0, pixel = 2^32 / 4096 ≈ 1,048,576 world units
        // A 100 world unit line = 0.0001 pixels - should be dropped
        let tile = TileCoord::new(0, 0, 0);
        let extent = 4096;

        let coords = vec![
            WorldCoord::new(2_000_000_000, 2_000_000_000),
            WorldCoord::new(2_000_000_100, 2_000_000_000), // 100 units horizontal
        ];

        let should_drop = should_drop_tiny_line_world(&coords, &tile, extent, 1.0);
        assert!(
            should_drop,
            "Tiny line (0.0001 pixels at zoom 0) should be dropped"
        );
    }

    #[test]
    fn test_world_line_empty_dropped() {
        let tile = TileCoord::new(0, 0, 10);
        let extent = 4096;

        let empty: Vec<WorldCoord> = vec![];
        assert!(
            should_drop_tiny_line_world(&empty, &tile, extent, 1.0),
            "Empty line should be dropped"
        );

        let single = vec![WorldCoord::new(100, 100)];
        assert!(
            should_drop_tiny_line_world(&single, &tile, extent, 1.0),
            "Single-point line should be dropped"
        );
    }

    #[test]
    fn test_world_line_zoom_sensitivity() {
        let extent = 4096;

        // A 10000 world unit line
        let coords = vec![
            WorldCoord::new(2_000_000_000, 2_000_000_000),
            WorldCoord::new(2_000_010_000, 2_000_000_000),
        ];

        // At high zoom (small pixels), line is visible
        let high_zoom_tile = TileCoord::new(0, 0, 20);
        assert!(
            !should_drop_tiny_line_world(&coords, &high_zoom_tile, extent, 1.0),
            "10000 world unit line should be visible at zoom 20"
        );

        // At low zoom (large pixels), same line may be dropped
        // At zoom 0, pixel = 2^32 / 4096 ≈ 1M world units
        // 10000 / 1M = 0.01 pixels
        let low_zoom_tile = TileCoord::new(0, 0, 0);
        assert!(
            should_drop_tiny_line_world(&coords, &low_zoom_tile, extent, 1.0),
            "10000 world unit line should be dropped at zoom 0"
        );
    }

    // =========================================================================
    // TinyPolygonAccumulator tests - Issue #85
    // =========================================================================
    //
    // These tests verify the tippecanoe-style tiny polygon accumulation strategy.
    // Instead of dropping tiny polygons, we accumulate their area and emit
    // synthetic pixel-sized squares when the accumulated area exceeds a threshold.
    // This preserves visual density - 10 tiny polygons become one visible square.
    //
    // Reference: tippecanoe clip.cpp:1048-1097

    #[test]
    fn test_tiny_polygon_accumulator_basic_creation() {
        // Create an accumulator with default threshold (4 sq pixels)
        let tile = TileCoord::new(0, 0, 14);
        let extent = 4096;
        let accumulator = TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        assert_eq!(accumulator.accumulated_area(), 0);
        assert!(!accumulator.should_emit());
    }

    #[test]
    fn test_tiny_polygon_accumulator_accumulates_area() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // At zoom 14, 1 pixel = 64 world units
        // 1 square pixel = 64 * 64 = 4096 square world units
        // Create a tiny polygon with area ~1000 sq world units (~0.24 sq pixels)
        let tiny_exterior = vec![
            WorldCoord::new(2_000_000_000, 2_000_000_000),
            WorldCoord::new(2_000_001_000, 2_000_000_000), // 1000 units right
            WorldCoord::new(2_000_001_000, 2_000_000_001), // 1 unit down
            WorldCoord::new(2_000_000_000, 2_000_000_001), // back
            WorldCoord::new(2_000_000_000, 2_000_000_000), // close
        ];

        accumulator.accumulate(&tiny_exterior, &[]);

        assert!(accumulator.accumulated_area() > 0);
    }

    #[test]
    fn test_tiny_polygon_accumulator_emits_when_threshold_exceeded() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        // Threshold of 4 sq pixels
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // At zoom 14, pixel = 64 world units, so 1 sq pixel = 4096 sq world units
        // Threshold is 4 sq pixels = 16384 sq world units
        // Create polygons that together exceed this threshold

        // Each polygon: 100 x 100 = 10000 sq world units (~2.4 sq pixels)
        for i in 0..3 {
            let offset = i * 200;
            let tiny_exterior = vec![
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
            ];
            accumulator.accumulate(&tiny_exterior, &[]);
        }

        // After 3 polygons of ~2.4 sq pixels each = ~7.2 sq pixels total
        // This exceeds the 4 sq pixel threshold
        assert!(
            accumulator.should_emit(),
            "Should emit after accumulated area exceeds threshold"
        );
    }

    #[test]
    fn test_tiny_polygon_accumulator_emits_synthetic_square() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // Add enough tiny polygons to trigger emission
        for i in 0..5 {
            let offset = i * 200;
            let tiny_exterior = vec![
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
            ];
            accumulator.accumulate(&tiny_exterior, &[]);
        }

        assert!(accumulator.should_emit());

        // Emit the synthetic square
        let synthetic = accumulator.emit_synthetic_square();
        assert!(synthetic.is_some(), "Should emit a synthetic square");

        let (exterior, interiors) = synthetic.unwrap();
        // Synthetic square should have 5 points (closed ring)
        assert_eq!(exterior.len(), 5, "Synthetic square should have 5 points");
        // Should have no holes
        assert!(
            interiors.is_empty(),
            "Synthetic square should have no holes"
        );
    }

    #[test]
    fn test_tiny_polygon_accumulator_resets_after_emission() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // Add enough to trigger emission
        for i in 0..5 {
            let offset = i * 200;
            let tiny_exterior = vec![
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
            ];
            accumulator.accumulate(&tiny_exterior, &[]);
        }

        // Emit
        let _ = accumulator.emit_synthetic_square();

        // After emission, accumulator should be reset
        assert_eq!(
            accumulator.accumulated_area(),
            0,
            "Accumulated area should reset after emission"
        );
        assert!(
            !accumulator.should_emit(),
            "Should not emit immediately after reset"
        );
    }

    #[test]
    fn test_tiny_polygon_accumulator_centroid_tracking() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // Add a single tiny polygon centered at a known location
        // Center should be at approximately (2_000_000_050, 2_000_000_050)
        let tiny_exterior = vec![
            WorldCoord::new(2_000_000_000, 2_000_000_000),
            WorldCoord::new(2_000_000_100, 2_000_000_000),
            WorldCoord::new(2_000_000_100, 2_000_000_100),
            WorldCoord::new(2_000_000_000, 2_000_000_100),
            WorldCoord::new(2_000_000_000, 2_000_000_000),
        ];

        // Add enough to exceed threshold
        for _ in 0..5 {
            accumulator.accumulate(&tiny_exterior, &[]);
        }

        let synthetic = accumulator.emit_synthetic_square();
        assert!(synthetic.is_some());

        let (exterior, _) = synthetic.unwrap();
        // The synthetic square should be centered near (2_000_000_050, 2_000_000_050)
        // Check that the center of the emitted square is close to expected
        let min_x = exterior.iter().map(|c| c.x).min().unwrap();
        let max_x = exterior.iter().map(|c| c.x).max().unwrap();
        let min_y = exterior.iter().map(|c| c.y).min().unwrap();
        let max_y = exterior.iter().map(|c| c.y).max().unwrap();
        let center_x = (min_x + max_x) / 2;
        let center_y = (min_y + max_y) / 2;

        // Should be within 100 world units of expected center
        assert!(
            (center_x as i64 - 2_000_000_050).abs() < 100,
            "Synthetic square center X should be near polygon centroid"
        );
        assert!(
            (center_y as i64 - 2_000_000_050).abs() < 100,
            "Synthetic square center Y should be near polygon centroid"
        );
    }

    #[test]
    fn test_tiny_polygon_accumulator_synthetic_square_is_one_pixel() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // Add enough tiny polygons to trigger emission
        for i in 0..5 {
            let offset = i * 200;
            let tiny_exterior = vec![
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
            ];
            accumulator.accumulate(&tiny_exterior, &[]);
        }

        let synthetic = accumulator.emit_synthetic_square();
        let (exterior, _) = synthetic.unwrap();

        // At zoom 14, 1 pixel = 64 world units
        // The synthetic square should be approximately 1 pixel in size
        let min_x = exterior.iter().map(|c| c.x).min().unwrap();
        let max_x = exterior.iter().map(|c| c.x).max().unwrap();
        let min_y = exterior.iter().map(|c| c.y).min().unwrap();
        let max_y = exterior.iter().map(|c| c.y).max().unwrap();

        let width = max_x - min_x;
        let height = max_y - min_y;

        // At zoom 14, 1 pixel = 64 world units
        // Allow some tolerance
        let pixel_size = 64u32;
        assert!(
            width >= pixel_size / 2 && width <= pixel_size * 2,
            "Synthetic square width should be approximately 1 pixel (64 world units), got {}",
            width
        );
        assert!(
            height >= pixel_size / 2 && height <= pixel_size * 2,
            "Synthetic square height should be approximately 1 pixel (64 world units), got {}",
            height
        );
    }

    #[test]
    fn test_tiny_polygon_accumulator_handles_holes() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // Polygon with a small hole - net area should still be positive
        let exterior = vec![
            WorldCoord::new(2_000_000_000, 2_000_000_000),
            WorldCoord::new(2_000_000_100, 2_000_000_000),
            WorldCoord::new(2_000_000_100, 2_000_000_100),
            WorldCoord::new(2_000_000_000, 2_000_000_100),
            WorldCoord::new(2_000_000_000, 2_000_000_000),
        ];

        // Small interior hole (10x10 = 100 sq units vs 10000 sq unit exterior)
        let interior = vec![
            WorldCoord::new(2_000_000_040, 2_000_000_040),
            WorldCoord::new(2_000_000_050, 2_000_000_040),
            WorldCoord::new(2_000_000_050, 2_000_000_050),
            WorldCoord::new(2_000_000_040, 2_000_000_050),
            WorldCoord::new(2_000_000_040, 2_000_000_040),
        ];

        accumulator.accumulate(&exterior, &[interior]);

        // Net area should be exterior - interior
        assert!(
            accumulator.accumulated_area() > 0,
            "Net area after subtracting hole should be positive"
        );
    }

    #[test]
    fn test_tiny_polygon_accumulator_no_emit_below_threshold() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        // Add just one tiny polygon - not enough to exceed threshold
        let tiny_exterior = vec![
            WorldCoord::new(2_000_000_000, 2_000_000_000),
            WorldCoord::new(2_000_000_010, 2_000_000_000),
            WorldCoord::new(2_000_000_010, 2_000_000_010),
            WorldCoord::new(2_000_000_000, 2_000_000_010),
            WorldCoord::new(2_000_000_000, 2_000_000_000),
        ];

        accumulator.accumulate(&tiny_exterior, &[]);

        assert!(
            !accumulator.should_emit(),
            "Should not emit when accumulated area is below threshold"
        );

        // Emit should return None when threshold not met
        let synthetic = accumulator.emit_synthetic_square();
        assert!(
            synthetic.is_none(),
            "emit_synthetic_square should return None below threshold"
        );
    }

    #[test]
    fn test_tiny_polygon_accumulator_multiple_emissions() {
        let tile = TileCoord::new(8192, 8192, 14);
        let extent = 4096;
        let mut accumulator =
            TinyPolygonAccumulator::new(tile, extent, DEFAULT_TINY_POLYGON_THRESHOLD);

        let mut emission_count = 0;

        // Add enough polygons for multiple emissions
        for i in 0..20 {
            let offset = i * 200;
            let tiny_exterior = vec![
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_000),
                WorldCoord::new(2_000_000_100 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_100),
                WorldCoord::new(2_000_000_000 + offset, 2_000_000_000),
            ];
            accumulator.accumulate(&tiny_exterior, &[]);

            if accumulator.should_emit() {
                let _ = accumulator.emit_synthetic_square();
                emission_count += 1;
            }
        }

        // With 20 polygons of ~2.4 sq pixels each = ~48 sq pixels total
        // At threshold of 4 sq pixels, we should get ~10-12 emissions
        assert!(
            emission_count >= 5,
            "Should emit multiple times for many tiny polygons, got {} emissions",
            emission_count
        );
    }
}
