//! Geometry coalescing for dense tiles.
//!
//! This module implements GeoParquet-native predictive coalescing that merges
//! geometries into Multi* types to reduce tile complexity without losing data.
//!
//! Unlike tippecanoe's reactive approach (encode → measure → retry), we predict
//! dense tiles upfront using row group metadata.

use crate::tile::TileBounds;
use geo::{BoundingRect, Centroid, Geometry};
use std::collections::{HashMap, HashSet};

// ============================================================================
// SpatialGrid - for O(1) cell assignment during coalescing
// ============================================================================

/// Grid size configuration for spatial coalescing.
#[derive(Debug, Clone)]
pub enum GridSize {
    /// Fixed grid size (e.g., always 4x4)
    Fixed(usize),
    /// Adaptive based on feature density
    Adaptive {
        /// Grid size for low-density tiles
        low: usize,
        /// Grid size for high-density tiles
        high: usize,
        /// Threshold (features/tile) to switch from low to high
        threshold: f64,
    },
}

impl Default for GridSize {
    fn default() -> Self {
        GridSize::Adaptive {
            low: 4,
            high: 8,
            threshold: 500.0,
        }
    }
}

/// Spatial grid for grouping features by location during coalescing.
///
/// Features are assigned to cells based on their centroid. Features in the
/// same cell are candidates for coalescing.
#[derive(Debug)]
pub struct SpatialGrid {
    /// Grid dimensions (size x size)
    size: usize,
    /// Tile bounds for coordinate mapping
    bounds: TileBounds,
}

impl SpatialGrid {
    /// Create a new spatial grid for the given tile bounds.
    ///
    /// # Arguments
    ///
    /// * `estimated_features` - Estimated number of features in this tile
    /// * `bounds` - Geographic bounds of the tile
    /// * `config` - Grid sizing configuration
    pub fn new(estimated_features: f64, bounds: TileBounds, config: &GridSize) -> Self {
        let size = match config {
            GridSize::Fixed(n) => *n,
            GridSize::Adaptive {
                low,
                high,
                threshold,
            } => {
                if estimated_features > *threshold {
                    *high
                } else {
                    *low
                }
            }
        };
        Self { size, bounds }
    }

    /// Get the grid dimensions.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Assign a geometry to a grid cell based on its centroid.
    ///
    /// Returns `None` if the centroid cannot be computed (e.g., empty geometry).
    /// Uses bounding rect center as fallback for degenerate cases.
    ///
    /// # Edge cases
    /// - Zero-width/height bounds: all features go to cell (0, 0)
    /// - Coordinates outside bounds: clamped to valid cell range
    pub fn assign_cell(&self, geom: &Geometry) -> Option<(usize, usize)> {
        // Guard against degenerate bounds (would cause division by zero)
        let width = self.bounds.width();
        let height = self.bounds.height();
        if width <= 0.0 || height <= 0.0 {
            return Some((0, 0)); // All features go to cell 0
        }

        // Primary: use centroid
        // Fallback: bounding rect center (handles degenerate cases)
        let center = geom
            .centroid()
            .map(|c| (c.x(), c.y()))
            .or_else(|| geom.bounding_rect().map(|r| (r.center().x, r.center().y)))?;

        let (cx, cy) = center;

        // Calculate cell coordinates with safe conversion
        // Handles negative results (coords outside bounds) by clamping to 0
        let x_raw = ((cx - self.bounds.lng_min) / width * self.size as f64).floor();
        let y_raw = ((cy - self.bounds.lat_min) / height * self.size as f64).floor();

        // Clamp to valid cell indices (handles both negative and overflow cases)
        let x = if x_raw < 0.0 {
            0
        } else {
            (x_raw as usize).min(self.size - 1)
        };
        let y = if y_raw < 0.0 {
            0
        } else {
            (y_raw as usize).min(self.size - 1)
        };

        Some((x, y))
    }
}

// ============================================================================
// CoalesceTargets - tracks which row groups need coalescing at which zooms
// ============================================================================

/// Tracks which row groups need coalescing at which zoom levels.
///
/// Built during the metadata scan phase, this structure enables O(1) lookup
/// during tile generation to determine if coalescing should be applied.
#[derive(Debug, Default)]
pub struct CoalesceTargets {
    /// Map of row_group_index -> set of zoom levels where it's dense
    dense_at: HashMap<usize, HashSet<u8>>,
    /// Density values for logging/debugging: (row_group, zoom) -> density
    densities: HashMap<(usize, u8), f64>,
}

impl CoalesceTargets {
    /// Create an empty CoalesceTargets.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a row group as dense at a specific zoom level.
    ///
    /// # Arguments
    ///
    /// * `row_group_idx` - Index of the row group
    /// * `zoom` - Zoom level where this row group exceeds density threshold
    /// * `density` - The computed density (features/tile) for debugging
    pub fn mark_dense(&mut self, row_group_idx: usize, zoom: u8, density: f64) {
        self.dense_at.entry(row_group_idx).or_default().insert(zoom);
        self.densities.insert((row_group_idx, zoom), density);
    }

    /// Check if a row group should be coalesced at a given zoom level.
    pub fn should_coalesce(&self, row_group_idx: usize, zoom: u8) -> bool {
        self.dense_at
            .get(&row_group_idx)
            .map(|zooms| zooms.contains(&zoom))
            .unwrap_or(false)
    }

    /// Get the density value for a row group at a zoom level (for debugging).
    pub fn get_density(&self, row_group_idx: usize, zoom: u8) -> Option<f64> {
        self.densities.get(&(row_group_idx, zoom)).copied()
    }

    /// Iterate over row group indices that are dense at a specific zoom level.
    pub fn dense_row_groups_at_zoom(&self, zoom: u8) -> impl Iterator<Item = usize> + '_ {
        self.dense_at
            .iter()
            .filter(move |(_, zooms)| zooms.contains(&zoom))
            .map(|(rg_idx, _)| *rg_idx)
    }

    /// Check if any row groups are marked as dense.
    pub fn is_empty(&self) -> bool {
        self.dense_at.is_empty()
    }

    /// Get the total number of (row_group, zoom) pairs marked as dense.
    pub fn total_dense_pairs(&self) -> usize {
        self.dense_at.values().map(|s| s.len()).sum()
    }
}

// ============================================================================
// CoalesceConfig - configuration for predictive coalescing
// ============================================================================

/// Attribute handling mode during geometry coalescing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AttributeMode {
    /// Drop attributes without configured accumulators (tippecanoe-compatible default)
    #[default]
    Drop,
    /// Keep first feature's value for unconfigured attributes
    KeepFirst,
    /// Error if any attribute lacks an accumulator config
    Strict,
}

/// Configuration for geometry coalescing.
///
/// Coalescing merges features into Multi* geometries to reduce tile complexity
/// while preserving all coordinate data. This is triggered predictively based
/// on GeoParquet row group metadata rather than reactively after tile encoding.
#[derive(Debug, Clone)]
pub struct CoalesceConfig {
    /// Percentile threshold for density-based coalescing (default: 90).
    ///
    /// Only the top (100 - percentile)% densest row groups are coalesced.
    /// 90 means only the top 10% densest row groups are coalesced.
    pub percentile: u8,

    /// Minimum features/tile to trigger coalescing (default: 100).
    ///
    /// Even if a row group exceeds the percentile threshold, coalescing is
    /// skipped if the estimated density is below this value.
    pub min_density_trigger: f64,

    /// Grid size configuration for spatial grouping.
    pub grid_size: GridSize,

    /// Attribute handling mode during coalescing.
    pub attribute_mode: AttributeMode,
}

impl Default for CoalesceConfig {
    fn default() -> Self {
        Self {
            percentile: 90,
            min_density_trigger: 100.0,
            grid_size: GridSize::default(),
            attribute_mode: AttributeMode::default(),
        }
    }
}

impl CoalesceConfig {
    /// Create a new config with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the density percentile threshold.
    pub fn with_percentile(mut self, percentile: u8) -> Self {
        self.percentile = percentile.min(100);
        self
    }

    /// Set the minimum density trigger.
    pub fn with_min_density(mut self, min_density: f64) -> Self {
        self.min_density_trigger = min_density;
        self
    }

    /// Set the grid size configuration.
    pub fn with_grid_size(mut self, grid_size: GridSize) -> Self {
        self.grid_size = grid_size;
        self
    }

    /// Set the attribute handling mode.
    pub fn with_attribute_mode(mut self, mode: AttributeMode) -> Self {
        self.attribute_mode = mode;
        self
    }
}

// ============================================================================
// Predictive coalescing: calculate targets from Parquet metadata
// ============================================================================

use crate::covering::{covering_tiles, extract_row_group_bounds};
use crate::gap_density::percentile;
use std::path::Path;

/// Estimate features-per-tile for a row group at a given zoom level.
///
/// # Arguments
///
/// * `bounds` - Geographic bounding box of the row group
/// * `num_rows` - Number of rows (features) in the row group
/// * `zoom` - Target zoom level
///
/// # Returns
///
/// Estimated number of features per tile at the given zoom level.
fn estimate_tile_density(bounds: &TileBounds, num_rows: usize, zoom: u8) -> f64 {
    let tile_count = covering_tiles(bounds, zoom).count().max(1);
    num_rows as f64 / tile_count as f64
}

/// Calculate coalesce targets from Parquet row group metadata.
///
/// This is the core of the predictive coalescing approach:
/// 1. Extract bounding boxes from each row group (no decompression)
/// 2. Estimate features-per-tile at each zoom level
/// 3. Find the percentile density threshold at each zoom
/// 4. Mark row groups exceeding that threshold as "dense"
///
/// # Arguments
///
/// * `path` - Path to the GeoParquet file
/// * `min_zoom` - Minimum zoom level to analyze
/// * `max_zoom` - Maximum zoom level to analyze
/// * `percentile_threshold` - Percentile (0-100) above which row groups are marked dense
/// * `min_density_trigger` - Minimum features/tile to trigger coalescing
///
/// # Returns
///
/// `Some(CoalesceTargets)` if the file has row group bbox metadata (gpio-optimized),
/// `None` if the file lacks covering metadata and predictive coalescing isn't possible.
///
/// # Example
///
/// ```ignore
/// use gpq_tiles_core::coalesce::calculate_coalesce_targets;
///
/// let targets = calculate_coalesce_targets(
///     Path::new("data.parquet"),
///     0,   // min_zoom
///     14,  // max_zoom
///     90,  // percentile (top 10% densest)
///     100.0, // min features/tile
/// );
///
/// if let Some(targets) = targets {
///     // Check if row group 5 should be coalesced at zoom 8
///     if targets.should_coalesce(5, 8) {
///         // Apply coalescing...
///     }
/// }
/// ```
pub fn calculate_coalesce_targets(
    path: &Path,
    min_zoom: u8,
    max_zoom: u8,
    percentile_threshold: u8,
    min_density_trigger: f64,
) -> Option<CoalesceTargets> {
    // Extract row group bounds from Parquet metadata
    let row_group_bounds = extract_row_group_bounds(path).ok()?;

    // If no row groups have bounds metadata, we can't do predictive coalescing
    let has_any_bounds = row_group_bounds.iter().any(|b| b.is_some());
    if !has_any_bounds {
        return None;
    }

    let mut targets = CoalesceTargets::new();

    for zoom in min_zoom..=max_zoom {
        // Calculate density for each row group at this zoom
        let densities: Vec<(usize, f64)> = row_group_bounds
            .iter()
            .enumerate()
            .filter_map(|(idx, bounds)| {
                let rgb = bounds.as_ref()?;
                let tile_bounds = TileBounds::new(rgb.xmin, rgb.ymin, rgb.xmax, rgb.ymax);
                let density = estimate_tile_density(&tile_bounds, rgb.num_rows, zoom);
                Some((idx, density))
            })
            .collect();

        // Need at least 5 row groups for stable percentile calculation
        if densities.len() < 5 {
            continue;
        }

        // Calculate threshold as specified percentile
        let density_values: Vec<f64> = densities.iter().map(|(_, d)| *d).collect();
        let threshold = percentile(&density_values, percentile_threshold as f64 / 100.0);

        // Mark row groups exceeding threshold (and min_density_trigger)
        for (rg_idx, density) in &densities {
            if *density > threshold && *density >= min_density_trigger {
                targets.mark_dense(*rg_idx, zoom, *density);
            }
        }
    }

    Some(targets)
}

// ============================================================================
// Coalescing result types
// ============================================================================

/// Result of attempting to coalesce two geometries.
#[derive(Debug)]
pub enum CoalesceResult {
    /// Geometries were merged into target
    Merged,
    /// Type mismatch - source should be kept as separate feature
    TypeMismatch(Geometry),
}

/// Coalesce source geometry into target, converting to Multi* as needed.
///
/// Geometries are only coalesced within the same "family":
/// - Point/MultiPoint
/// - LineString/MultiLineString/Line
/// - Polygon/MultiPolygon/Rect/Triangle
///
/// Type mismatches return `CoalesceResult::TypeMismatch` with the source geometry.
///
/// # Arguments
///
/// * `target` - Mutable reference to the target geometry (will be modified)
/// * `source` - Source geometry to coalesce into target
///
/// # Returns
///
/// `CoalesceResult::Merged` if successful, `CoalesceResult::TypeMismatch(source)` otherwise.
pub fn coalesce_geometries(target: &mut Geometry, source: Geometry) -> CoalesceResult {
    use Geometry::*;

    // Convert target from Line/Rect/Triangle to their canonical forms
    // This must happen BEFORE matching so patterns work correctly
    match target {
        Line(l) => {
            *target = LineString((*l).into());
        }
        Rect(r) => {
            *target = Polygon(r.to_polygon());
        }
        Triangle(t) => {
            *target = Polygon(t.to_polygon());
        }
        _ => {}
    }

    // Handle convertible source types
    let source = match source {
        Line(l) => LineString(l.into()),
        Rect(r) => Polygon(r.to_polygon()),
        Triangle(t) => Polygon(t.to_polygon()),
        other => other,
    };

    // Handle GeometryCollection separately
    if let GeometryCollection(gc) = source {
        let mut unmerged = Vec::new();
        for geom in gc.0 {
            if let CoalesceResult::TypeMismatch(g) = coalesce_geometries(target, geom) {
                unmerged.push(g);
            }
        }
        return if unmerged.is_empty() {
            CoalesceResult::Merged
        } else if unmerged.len() == 1 {
            CoalesceResult::TypeMismatch(unmerged.remove(0))
        } else {
            CoalesceResult::TypeMismatch(GeometryCollection(geo::GeometryCollection::new_from(
                unmerged,
            )))
        };
    }

    match (&*target, source) {
        // === Point family ===
        (Point(p1), Point(p2)) => {
            *target = MultiPoint(geo::MultiPoint::new(vec![*p1, p2]));
            CoalesceResult::Merged
        }
        (MultiPoint(_), Point(p)) => {
            if let MultiPoint(mp) = target {
                mp.0.push(p);
            }
            CoalesceResult::Merged
        }
        (Point(p1), MultiPoint(mp2)) => {
            let mut points = vec![*p1];
            points.extend(mp2.0);
            *target = MultiPoint(geo::MultiPoint::new(points));
            CoalesceResult::Merged
        }
        (MultiPoint(_), MultiPoint(mp2)) => {
            if let MultiPoint(mp1) = target {
                mp1.0.extend(mp2.0);
            }
            CoalesceResult::Merged
        }

        // === LineString family ===
        (LineString(l1), LineString(l2)) => {
            let l1_clone = l1.clone();
            *target = MultiLineString(geo::MultiLineString::new(vec![l1_clone, l2]));
            CoalesceResult::Merged
        }
        (MultiLineString(_), LineString(l)) => {
            if let MultiLineString(ml) = target {
                ml.0.push(l);
            }
            CoalesceResult::Merged
        }
        (LineString(l1), MultiLineString(ml2)) => {
            let mut lines = vec![l1.clone()];
            lines.extend(ml2.0);
            *target = MultiLineString(geo::MultiLineString::new(lines));
            CoalesceResult::Merged
        }
        (MultiLineString(_), MultiLineString(ml2)) => {
            if let MultiLineString(ml1) = target {
                ml1.0.extend(ml2.0);
            }
            CoalesceResult::Merged
        }

        // === Polygon family ===
        (Polygon(p1), Polygon(p2)) => {
            let p1_clone = p1.clone();
            *target = MultiPolygon(geo::MultiPolygon::new(vec![p1_clone, p2]));
            CoalesceResult::Merged
        }
        (MultiPolygon(_), Polygon(p)) => {
            if let MultiPolygon(mp) = target {
                mp.0.push(p);
            }
            CoalesceResult::Merged
        }
        (Polygon(p1), MultiPolygon(mp2)) => {
            let mut polys = vec![p1.clone()];
            polys.extend(mp2.0);
            *target = MultiPolygon(geo::MultiPolygon::new(polys));
            CoalesceResult::Merged
        }
        (MultiPolygon(_), MultiPolygon(mp2)) => {
            if let MultiPolygon(mp1) = target {
                mp1.0.extend(mp2.0);
            }
            CoalesceResult::Merged
        }

        // === Type mismatch: return source unchanged ===
        (_, source) => CoalesceResult::TypeMismatch(source),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{
        coord, point, polygon, GeometryCollection, Line, LineString, MultiPoint, Rect, Triangle,
    };

    // =========================================================================
    // Point family coalescing
    // =========================================================================

    #[test]
    fn test_point_plus_point_becomes_multipoint() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::Point(point!(x: 1.0, y: 1.0));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 2);
                assert_eq!(mp.0[0], point!(x: 0.0, y: 0.0));
                assert_eq!(mp.0[1], point!(x: 1.0, y: 1.0));
            }
            _ => panic!("Expected MultiPoint, got {:?}", target),
        }
    }

    #[test]
    fn test_multipoint_plus_point_extends() {
        let mut target = Geometry::MultiPoint(MultiPoint::new(vec![
            point!(x: 0.0, y: 0.0),
            point!(x: 1.0, y: 1.0),
        ]));
        let source = Geometry::Point(point!(x: 2.0, y: 2.0));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
            }
            _ => panic!("Expected MultiPoint"),
        }
    }

    #[test]
    fn test_multipoint_plus_multipoint_merges() {
        let mut target = Geometry::MultiPoint(MultiPoint::new(vec![point!(x: 0.0, y: 0.0)]));
        let source = Geometry::MultiPoint(MultiPoint::new(vec![
            point!(x: 1.0, y: 1.0),
            point!(x: 2.0, y: 2.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
            }
            _ => panic!("Expected MultiPoint"),
        }
    }

    // =========================================================================
    // LineString family coalescing
    // =========================================================================

    #[test]
    fn test_linestring_plus_linestring_becomes_multilinestring() {
        let mut target = Geometry::LineString(LineString::new(vec![
            coord!(x: 0.0, y: 0.0),
            coord!(x: 1.0, y: 1.0),
        ]));
        let source = Geometry::LineString(LineString::new(vec![
            coord!(x: 2.0, y: 2.0),
            coord!(x: 3.0, y: 3.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 2);
            }
            _ => panic!("Expected MultiLineString"),
        }
    }

    #[test]
    fn test_line_coalesces_as_linestring() {
        let mut target = Geometry::LineString(LineString::new(vec![
            coord!(x: 0.0, y: 0.0),
            coord!(x: 1.0, y: 1.0),
        ]));
        let source = Geometry::Line(Line::new(coord!(x: 2.0, y: 2.0), coord!(x: 3.0, y: 3.0)));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 2);
            }
            _ => panic!("Expected MultiLineString"),
        }
    }

    // =========================================================================
    // Polygon family coalescing
    // =========================================================================

    #[test]
    fn test_polygon_plus_polygon_becomes_multipolygon() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Polygon(polygon![
            (x: 2.0, y: 2.0),
            (x: 3.0, y: 2.0),
            (x: 3.0, y: 3.0),
            (x: 2.0, y: 3.0),
            (x: 2.0, y: 2.0),
        ]);

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }

    #[test]
    fn test_rect_coalesces_as_polygon() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Rect(Rect::new(coord!(x: 2.0, y: 2.0), coord!(x: 3.0, y: 3.0)));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }

    #[test]
    fn test_triangle_coalesces_as_polygon() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Triangle(Triangle::new(
            coord!(x: 2.0, y: 2.0),
            coord!(x: 3.0, y: 2.0),
            coord!(x: 2.5, y: 3.0),
        ));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon"),
        }
    }

    // =========================================================================
    // Type mismatch handling
    // =========================================================================

    #[test]
    fn test_point_plus_linestring_mismatch() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::LineString(LineString::new(vec![
            coord!(x: 1.0, y: 1.0),
            coord!(x: 2.0, y: 2.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        match result {
            CoalesceResult::TypeMismatch(g) => {
                assert!(matches!(g, Geometry::LineString(_)));
            }
            _ => panic!("Expected TypeMismatch"),
        }
        // Target should be unchanged
        assert!(matches!(target, Geometry::Point(_)));
    }

    #[test]
    fn test_polygon_plus_point_mismatch() {
        let mut target = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let source = Geometry::Point(point!(x: 5.0, y: 5.0));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::TypeMismatch(_)));
    }

    // =========================================================================
    // GeometryCollection handling
    // =========================================================================

    #[test]
    fn test_geometry_collection_flattens_and_coalesces() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::GeometryCollection(GeometryCollection::new_from(vec![
            Geometry::Point(point!(x: 1.0, y: 1.0)),
            Geometry::Point(point!(x: 2.0, y: 2.0)),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
            }
            _ => panic!("Expected MultiPoint with 3 points"),
        }
    }

    #[test]
    fn test_geometry_collection_with_mixed_types_returns_unmerged() {
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::GeometryCollection(GeometryCollection::new_from(vec![
            Geometry::Point(point!(x: 1.0, y: 1.0)),
            Geometry::LineString(LineString::new(vec![
                coord!(x: 2.0, y: 2.0),
                coord!(x: 3.0, y: 3.0),
            ])),
        ]));

        let result = coalesce_geometries(&mut target, source);

        // The point should be merged, but the linestring should be returned as mismatch
        match result {
            CoalesceResult::TypeMismatch(g) => {
                // Should contain the linestring that couldn't be merged
                match g {
                    Geometry::GeometryCollection(gc) => {
                        assert_eq!(gc.0.len(), 1);
                        assert!(matches!(gc.0[0], Geometry::LineString(_)));
                    }
                    Geometry::LineString(_) => {
                        // Also acceptable if only one unmerged
                    }
                    _ => panic!("Expected unmerged geometries"),
                }
            }
            CoalesceResult::Merged => {
                // If all merged, target should be MultiPoint with the point only
                // (but this shouldn't happen with mixed types)
                panic!("Expected TypeMismatch for mixed GeometryCollection");
            }
        }
    }

    // =========================================================================
    // SpatialGrid tests
    // =========================================================================

    #[test]
    fn test_spatial_grid_creation() {
        let bounds = crate::tile::TileBounds::new(-122.5, 37.7, -122.3, 37.9);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::default());

        // Default adaptive: 100 features < 500 threshold → 4x4 grid
        assert_eq!(grid.size(), 4);
    }

    #[test]
    fn test_spatial_grid_high_density_uses_larger_grid() {
        let bounds = crate::tile::TileBounds::new(-122.5, 37.7, -122.3, 37.9);
        let grid = SpatialGrid::new(1000.0, bounds, &GridSize::default());

        // 1000 features > 500 threshold → 8x8 grid
        assert_eq!(grid.size(), 8);
    }

    #[test]
    fn test_spatial_grid_fixed_size() {
        let bounds = crate::tile::TileBounds::new(-122.5, 37.7, -122.3, 37.9);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(6));

        assert_eq!(grid.size(), 6);
    }

    #[test]
    fn test_spatial_grid_assigns_cell_correctly() {
        // Grid covering 0-10 in both dimensions
        let bounds = crate::tile::TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Point at (2.5, 2.5) should be in cell (1, 1) of a 4x4 grid
        let point = Geometry::Point(point!(x: 2.5, y: 2.5));
        let cell = grid.assign_cell(&point);

        assert!(cell.is_some());
        let (x, y) = cell.unwrap();
        assert_eq!(
            x, 1,
            "Expected x=1 for point at x=2.5 in 4x4 grid over 0-10"
        );
        assert_eq!(
            y, 1,
            "Expected y=1 for point at y=2.5 in 4x4 grid over 0-10"
        );
    }

    #[test]
    fn test_spatial_grid_boundary_cases() {
        let bounds = crate::tile::TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Point at origin
        let origin = Geometry::Point(point!(x: 0.0, y: 0.0));
        assert_eq!(grid.assign_cell(&origin), Some((0, 0)));

        // Point at max corner (should clamp to last cell)
        let max_corner = Geometry::Point(point!(x: 10.0, y: 10.0));
        let cell = grid.assign_cell(&max_corner);
        assert!(cell.is_some());
        let (x, y) = cell.unwrap();
        assert!(x <= 3, "Should clamp to grid bounds");
        assert!(y <= 3, "Should clamp to grid bounds");
    }

    #[test]
    fn test_spatial_grid_centroid_fallback() {
        let bounds = crate::tile::TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Polygon uses centroid
        let poly = Geometry::Polygon(polygon![
            (x: 2.0, y: 2.0),
            (x: 3.0, y: 2.0),
            (x: 3.0, y: 3.0),
            (x: 2.0, y: 3.0),
            (x: 2.0, y: 2.0),
        ]);
        let cell = grid.assign_cell(&poly);
        assert!(cell.is_some());
    }

    // =========================================================================
    // CoalesceTargets tests
    // =========================================================================

    #[test]
    fn test_coalesce_targets_empty() {
        let targets = CoalesceTargets::new();
        assert!(!targets.should_coalesce(0, 10));
        assert!(!targets.should_coalesce(5, 5));
    }

    #[test]
    fn test_coalesce_targets_mark_and_query() {
        let mut targets = CoalesceTargets::new();

        // Mark row group 3 as dense at zoom 8
        targets.mark_dense(3, 8, 1500.0);

        assert!(targets.should_coalesce(3, 8));
        assert!(!targets.should_coalesce(3, 10)); // Different zoom
        assert!(!targets.should_coalesce(4, 8)); // Different row group
    }

    #[test]
    fn test_coalesce_targets_multiple_zooms() {
        let mut targets = CoalesceTargets::new();

        // Row group 0 is dense at zooms 4, 5, 6
        targets.mark_dense(0, 4, 2000.0);
        targets.mark_dense(0, 5, 1500.0);
        targets.mark_dense(0, 6, 1200.0);

        assert!(targets.should_coalesce(0, 4));
        assert!(targets.should_coalesce(0, 5));
        assert!(targets.should_coalesce(0, 6));
        assert!(!targets.should_coalesce(0, 7)); // Not marked
    }

    #[test]
    fn test_coalesce_targets_density_tracking() {
        let mut targets = CoalesceTargets::new();

        targets.mark_dense(2, 10, 850.5);

        // Should be able to retrieve the density for debugging
        assert_eq!(targets.get_density(2, 10), Some(850.5));
        assert_eq!(targets.get_density(2, 11), None);
    }

    #[test]
    fn test_coalesce_targets_dense_row_groups_at_zoom() {
        let mut targets = CoalesceTargets::new();

        targets.mark_dense(0, 5, 1000.0);
        targets.mark_dense(2, 5, 1500.0);
        targets.mark_dense(5, 5, 2000.0);
        targets.mark_dense(2, 6, 800.0); // Different zoom

        let dense_at_5: Vec<_> = targets.dense_row_groups_at_zoom(5).collect();
        assert_eq!(dense_at_5.len(), 3);
        assert!(dense_at_5.contains(&0));
        assert!(dense_at_5.contains(&2));
        assert!(dense_at_5.contains(&5));
    }

    // =========================================================================
    // Edge case tests (bug fixes)
    // =========================================================================

    #[test]
    fn test_spatial_grid_zero_width_bounds() {
        // Degenerate bounds with zero width (single longitude line)
        let bounds = crate::tile::TileBounds::new(5.0, 0.0, 5.0, 10.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Should not panic, all features go to cell (0, 0)
        let point = Geometry::Point(point!(x: 5.0, y: 5.0));
        let cell = grid.assign_cell(&point);
        assert_eq!(cell, Some((0, 0)));
    }

    #[test]
    fn test_spatial_grid_zero_height_bounds() {
        // Degenerate bounds with zero height (single latitude line)
        let bounds = crate::tile::TileBounds::new(0.0, 5.0, 10.0, 5.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Should not panic, all features go to cell (0, 0)
        let point = Geometry::Point(point!(x: 5.0, y: 5.0));
        let cell = grid.assign_cell(&point);
        assert_eq!(cell, Some((0, 0)));
    }

    #[test]
    fn test_spatial_grid_point_outside_bounds_negative() {
        let bounds = crate::tile::TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Point with negative coordinates (outside bounds to the left/bottom)
        let point = Geometry::Point(point!(x: -5.0, y: -3.0));
        let cell = grid.assign_cell(&point);

        // Should clamp to (0, 0), not overflow
        assert_eq!(cell, Some((0, 0)));
    }

    #[test]
    fn test_spatial_grid_point_far_outside_bounds() {
        let bounds = crate::tile::TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Point way outside bounds
        let point = Geometry::Point(point!(x: 1000.0, y: 1000.0));
        let cell = grid.assign_cell(&point);

        // Should clamp to max cell (3, 3)
        assert_eq!(cell, Some((3, 3)));
    }

    #[test]
    fn test_point_plus_multipoint_coalesces() {
        // Test case: Point (target) + MultiPoint (source)
        let mut target = Geometry::Point(point!(x: 0.0, y: 0.0));
        let source = Geometry::MultiPoint(MultiPoint::new(vec![
            point!(x: 1.0, y: 1.0),
            point!(x: 2.0, y: 2.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3, "Should have original point + 2 from source");
            }
            _ => panic!("Expected MultiPoint"),
        }
    }

    #[test]
    fn test_line_as_target_coalesces_with_linestring() {
        // Test case: Line (target) + LineString (source)
        // This verifies the target conversion fix
        let mut target = Geometry::Line(Line::new(coord!(x: 0.0, y: 0.0), coord!(x: 1.0, y: 1.0)));
        let source = Geometry::LineString(LineString::new(vec![
            coord!(x: 2.0, y: 2.0),
            coord!(x: 3.0, y: 3.0),
        ]));

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiLineString(mls) => {
                assert_eq!(mls.0.len(), 2);
            }
            _ => panic!("Expected MultiLineString, got {:?}", target),
        }
    }

    #[test]
    fn test_rect_as_target_coalesces_with_polygon() {
        // Test case: Rect (target) + Polygon (source)
        let mut target = Geometry::Rect(Rect::new(coord!(x: 0.0, y: 0.0), coord!(x: 1.0, y: 1.0)));
        let source = Geometry::Polygon(polygon![
            (x: 2.0, y: 2.0),
            (x: 3.0, y: 2.0),
            (x: 3.0, y: 3.0),
            (x: 2.0, y: 3.0),
            (x: 2.0, y: 2.0),
        ]);

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon, got {:?}", target),
        }
    }

    #[test]
    fn test_triangle_as_target_coalesces_with_polygon() {
        // Test case: Triangle (target) + Polygon (source)
        let mut target = Geometry::Triangle(Triangle::new(
            coord!(x: 0.0, y: 0.0),
            coord!(x: 1.0, y: 0.0),
            coord!(x: 0.5, y: 1.0),
        ));
        let source = Geometry::Polygon(polygon![
            (x: 2.0, y: 2.0),
            (x: 3.0, y: 2.0),
            (x: 3.0, y: 3.0),
            (x: 2.0, y: 3.0),
            (x: 2.0, y: 2.0),
        ]);

        let result = coalesce_geometries(&mut target, source);

        assert!(matches!(result, CoalesceResult::Merged));
        match &target {
            Geometry::MultiPolygon(mp) => {
                assert_eq!(mp.0.len(), 2);
            }
            _ => panic!("Expected MultiPolygon, got {:?}", target),
        }
    }

    #[test]
    fn test_empty_multipoint_returns_none_for_cell() {
        let bounds = crate::tile::TileBounds::new(0.0, 0.0, 10.0, 10.0);
        let grid = SpatialGrid::new(100.0, bounds, &GridSize::Fixed(4));

        // Empty MultiPoint has no centroid or bounding rect
        let empty_mp = Geometry::MultiPoint(MultiPoint::new(vec![]));
        let cell = grid.assign_cell(&empty_mp);

        // Should return None (no cell assignable)
        assert!(cell.is_none());
    }

    // =========================================================================
    // Predictive coalescing: density estimation tests
    // =========================================================================

    #[test]
    fn test_estimate_tile_density_single_tile() {
        // Bounds that fit in a single tile at z10
        let bounds = TileBounds::new(-122.42, 37.78, -122.40, 37.80);
        let density = estimate_tile_density(&bounds, 1000, 10);

        // Should be ~1000 features/tile since bounds fits in ~1 tile
        assert!(
            density >= 500.0 && density <= 1500.0,
            "Expected ~1000, got {}",
            density
        );
    }

    #[test]
    fn test_estimate_tile_density_multiple_tiles() {
        // Bounds spanning multiple tiles at z10
        let bounds = TileBounds::new(-122.5, 37.5, -122.0, 38.0);
        let density = estimate_tile_density(&bounds, 1000, 10);

        // Should be lower since features spread across multiple tiles
        assert!(
            density < 500.0,
            "Expected density < 500 (spread across tiles), got {}",
            density
        );
    }

    #[test]
    fn test_estimate_tile_density_zoom_scaling() {
        let bounds = TileBounds::new(-122.5, 37.5, -122.0, 38.0);

        let density_z8 = estimate_tile_density(&bounds, 1000, 8);
        let density_z12 = estimate_tile_density(&bounds, 1000, 12);

        // Higher zoom = more tiles = lower density per tile
        assert!(
            density_z8 > density_z12,
            "Expected z8 density ({}) > z12 density ({})",
            density_z8,
            density_z12
        );
    }
}
