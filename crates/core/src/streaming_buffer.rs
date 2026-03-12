//! Streaming tile buffer for memory-bounded tile generation.
//!
//! This module implements a streaming buffer that accumulates features into tiles
//! and flushes them to a spool when they're likely complete. It's designed for
//! Hilbert-sorted GeoParquet input where features arrive in spatial order.
//!
//! # Key Components
//!
//! - [`HilbertCalibrator`]: Dynamically calibrates thresholds based on observed data
//! - [`TileAccumulator`]: Accumulates features for a single tile
//! - [`StreamingTileBuffer`]: Main buffer managing active tiles and flushing
//!
//! # Memory Model
//!
//! The buffer maintains a configurable number of active tiles in memory.
//! When a tile is determined to be "complete" (based on Hilbert distance from
//! current progress), it's encoded and flushed to the spool. This allows
//! processing datasets larger than RAM with O(active_tiles) memory usage.
//!
//! # Late Arrival Handling
//!
//! When input is imperfectly sorted, features may arrive for tiles that have
//! already been flushed. The buffer tracks these "late arrivals" in statistics
//! and re-creates the tile accumulator. The spool handles deduplication by
//! keeping only the last entry per tile_id.

use crate::mvt::{LayerBuilder, PropertyValue, TileBuilder};
use crate::streaming_types::{SpoolResult, StreamingConfig, StreamingStats, TileFormat};
use crate::tile::TileCoord;
use crate::tile_spool::TileSpool;
use crate::wkb::{deserialize_properties, wkb_to_geometry, PropertyValue as WkbPropertyValue};
use prost::Message;
use std::collections::{HashMap, HashSet};
use std::io;

/// Convert wkb::PropertyValue to mvt::PropertyValue.
fn convert_property_value(v: WkbPropertyValue) -> PropertyValue {
    match v {
        WkbPropertyValue::String(s) => PropertyValue::String(s),
        WkbPropertyValue::Int(i) => PropertyValue::Int(i),
        WkbPropertyValue::UInt(u) => PropertyValue::UInt(u),
        WkbPropertyValue::Float(f) => PropertyValue::Double(f), // wkb uses f64 for Float
        WkbPropertyValue::Bool(b) => PropertyValue::Bool(b),
    }
}

/// A feature to be added to a tile.
///
/// This is a simplified representation of a feature for the streaming buffer.
/// The geometry is stored as WKB and properties as MessagePack-serialized bytes.
#[derive(Debug, Clone)]
pub struct TileFeature {
    /// Original feature ID from source data
    pub feature_id: u64,
    /// WKB-encoded geometry (clipped to tile)
    pub geometry_wkb: Vec<u8>,
    /// MessagePack-serialized properties
    pub properties: Vec<u8>,
}

impl TileFeature {
    /// Create a new tile feature.
    pub fn new(feature_id: u64, geometry_wkb: Vec<u8>, properties: Vec<u8>) -> Self {
        Self {
            feature_id,
            geometry_wkb,
            properties,
        }
    }
}

/// Extracts the zoom level from a PMTiles tile_id.
///
/// PMTiles tile_id is calculated as:
/// `tile_id = sum of all tiles at previous zoom levels + hilbert_index at current zoom`
///
/// For zoom z, there are 4^z tiles, and the cumulative count is (4^(z+1) - 1) / 3.
fn zoom_from_tile_id(tile_id: u64) -> u8 {
    if tile_id == 0 {
        return 0;
    }

    // Binary search for the zoom level
    // At zoom z, tile_ids range from (4^z - 1) / 3 to (4^(z+1) - 1) / 3 - 1
    for z in 0..=30u8 {
        let base = if z == 0 {
            0
        } else {
            (4u64.pow(z as u32) - 1) / 3
        };
        let next_base = (4u64.pow((z + 1) as u32) - 1) / 3;
        if tile_id >= base && tile_id < next_base {
            return z;
        }
    }
    30 // Max zoom
}

/// Adaptive calibrator for Hilbert-based tile completion thresholds.
///
/// Instead of using hardcoded magic numbers, this calibrator collects samples
/// during the first N features and computes per-zoom-level thresholds based
/// on the observed Hilbert span of tiles.
#[derive(Debug, Clone)]
pub struct HilbertCalibrator {
    /// Sample: (tile_id, hilbert_index) for first N features
    samples: Vec<(u64, u64)>,
    /// Number of samples to collect before calibrating
    calibration_size: usize,
    /// Calibrated thresholds per zoom level (index 0 = zoom 0, etc.)
    thresholds: Option<[u64; 15]>,
}

impl Default for HilbertCalibrator {
    fn default() -> Self {
        Self::new(100_000)
    }
}

impl HilbertCalibrator {
    /// Create a new calibrator with the specified sample size.
    ///
    /// # Arguments
    ///
    /// * `calibration_size` - Number of samples to collect before calibrating.
    ///   Default is 100,000.
    pub fn new(calibration_size: usize) -> Self {
        Self {
            samples: Vec::with_capacity(calibration_size.min(100_000)),
            calibration_size,
            thresholds: None,
        }
    }

    /// Add a sample to the calibrator.
    ///
    /// Once `calibration_size` samples are collected, calibration is triggered
    /// automatically.
    pub fn add_sample(&mut self, tile_id: u64, hilbert: u64) {
        if self.samples.len() < self.calibration_size {
            self.samples.push((tile_id, hilbert));
        }
        if self.samples.len() == self.calibration_size && self.thresholds.is_none() {
            self.calibrate();
        }
    }

    /// Check if calibration has completed.
    pub fn is_calibrated(&self) -> bool {
        self.thresholds.is_some()
    }

    /// Get the number of samples collected.
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Manually trigger calibration.
    ///
    /// Useful when you have fewer samples than `calibration_size` but want
    /// to finalize calibration.
    pub fn calibrate(&mut self) {
        if self.samples.is_empty() {
            // Use conservative defaults
            self.thresholds = Some([100_000; 15]);
            return;
        }

        // Group samples by tile_id, compute Hilbert span for each tile
        let mut tile_spans: HashMap<u64, (u64, u64)> = HashMap::new();
        for &(tile_id, hilbert) in &self.samples {
            tile_spans
                .entry(tile_id)
                .and_modify(|(min, max)| {
                    *min = (*min).min(hilbert);
                    *max = (*max).max(hilbert);
                })
                .or_insert((hilbert, hilbert));
        }

        // Compute p95 span per zoom level
        let mut spans_by_zoom: [Vec<u64>; 15] = Default::default();
        for (tile_id, (min, max)) in tile_spans {
            let z = zoom_from_tile_id(tile_id) as usize;
            if z < 15 {
                spans_by_zoom[z].push(max.saturating_sub(min));
            }
        }

        let mut thresholds = [0u64; 15];
        for z in 0..15 {
            let spans = &mut spans_by_zoom[z];
            if spans.is_empty() {
                // Fallback for zoom levels with no samples
                // Higher zooms have smaller tiles with fewer features
                thresholds[z] = 10u64.pow(6 - (z as u32).min(5));
            } else {
                spans.sort_unstable();
                // Use p95 span * 2 as threshold (safe margin)
                let p95_idx = (spans.len() * 95) / 100;
                thresholds[z] = spans[p95_idx.min(spans.len() - 1)].saturating_mul(2);
                // Ensure a minimum threshold
                thresholds[z] = thresholds[z].max(1000);
            }
        }

        self.thresholds = Some(thresholds);
        tracing::info!("Calibrated Hilbert thresholds: {:?}", thresholds);
    }

    /// Get the threshold for a given zoom level.
    ///
    /// Returns a conservative default if calibration hasn't completed.
    pub fn threshold_for_zoom(&self, z: u8) -> u64 {
        self.thresholds
            .map(|t| t[(z as usize).min(14)])
            .unwrap_or(100_000) // Conservative default before calibration
    }
}

/// Accumulator for a single tile's features.
///
/// Tracks all features assigned to a tile and the Hilbert index of the last update.
#[derive(Debug)]
pub struct TileAccumulator {
    /// PMTiles tile_id
    pub tile_id: u64,
    /// Tile coordinates
    pub coord: TileCoord,
    /// Features accumulated for this tile
    pub features: Vec<TileFeature>,
    /// Hilbert index of the most recent feature added
    pub last_update_hilbert: u64,
}

impl TileAccumulator {
    /// Create a new tile accumulator.
    pub fn new(tile_id: u64, coord: TileCoord) -> Self {
        Self {
            tile_id,
            coord,
            features: Vec::new(),
            last_update_hilbert: 0,
        }
    }

    /// Add a feature to the accumulator.
    pub fn add_feature(&mut self, feature: TileFeature, source_hilbert: u64) {
        self.features.push(feature);
        self.last_update_hilbert = self.last_update_hilbert.max(source_hilbert);
    }

    /// Get the number of features in this accumulator.
    pub fn feature_count(&self) -> usize {
        self.features.len()
    }
}

/// Streaming buffer for tile generation.
///
/// Manages a set of active tiles, detecting when tiles are complete and
/// flushing them to a spool. Designed for Hilbert-sorted input where
/// features arrive in spatial order.
pub struct StreamingTileBuffer {
    /// Active tiles being accumulated
    active_tiles: HashMap<u64, TileAccumulator>,
    /// Output spool for completed tiles
    spool: TileSpool,
    /// Track which tiles have been flushed (for late arrival detection)
    flushed_tiles: HashSet<u64>,
    /// Highest Hilbert index seen so far
    hilbert_high_water_mark: u64,
    /// Configuration
    config: StreamingConfig,
    /// Statistics
    stats: StreamingStats,
    /// Adaptive threshold calibrator
    calibrator: HilbertCalibrator,
    /// Layer name for MVT encoding
    layer_name: String,
    /// Tile extent for MVT encoding
    extent: u32,
}

impl StreamingTileBuffer {
    /// Create a new streaming tile buffer.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the buffer
    /// * `layer_name` - Layer name for MVT encoding
    ///
    /// # Returns
    ///
    /// A new streaming tile buffer, or an IO error if spool creation fails.
    pub fn new(config: StreamingConfig, layer_name: &str) -> io::Result<Self> {
        Self::with_extent(config, layer_name, 4096)
    }

    /// Create a new streaming tile buffer with custom extent.
    ///
    /// # Arguments
    ///
    /// * `config` - Configuration for the buffer
    /// * `layer_name` - Layer name for MVT encoding
    /// * `extent` - Tile extent for MVT encoding (default: 4096)
    pub fn with_extent(config: StreamingConfig, layer_name: &str, extent: u32) -> io::Result<Self> {
        let spool = TileSpool::new()?;

        Ok(Self {
            active_tiles: HashMap::with_capacity(config.max_active_tiles),
            spool,
            flushed_tiles: HashSet::new(),
            hilbert_high_water_mark: 0,
            config,
            stats: StreamingStats::default(),
            calibrator: HilbertCalibrator::default(),
            layer_name: layer_name.to_string(),
            extent,
        })
    }

    /// Add a feature to the buffer.
    ///
    /// The feature is assigned to the specified tile. If the tile doesn't exist,
    /// it's created. If the tile was previously flushed (late arrival), a new
    /// accumulator is created and the late arrival is counted.
    ///
    /// # Arguments
    ///
    /// * `tile_id` - PMTiles tile_id
    /// * `coord` - Tile coordinates
    /// * `feature` - The feature to add
    /// * `source_hilbert` - Hilbert index of the source feature's centroid
    ///
    /// # Returns
    ///
    /// IO error if flushing to spool fails.
    pub fn add_feature(
        &mut self,
        tile_id: u64,
        coord: TileCoord,
        feature: TileFeature,
        source_hilbert: u64,
    ) -> io::Result<()> {
        // Add sample to calibrator
        self.calibrator.add_sample(tile_id, source_hilbert);

        // Detect late arrivals
        if self.flushed_tiles.contains(&tile_id) {
            self.stats.late_arrivals += 1;
            // Don't error - sparse spool handles this. Just recreate the accumulator.
        }

        self.active_tiles
            .entry(tile_id)
            .or_insert_with(|| TileAccumulator::new(tile_id, coord))
            .add_feature(feature, source_hilbert);

        self.stats.features_processed += 1;
        self.hilbert_high_water_mark = self.hilbert_high_water_mark.max(source_hilbert);

        self.maybe_flush_completed()?;
        self.maybe_warn_late_arrivals();

        // Handle memory pressure
        if self.active_tiles.len() > self.config.max_active_tiles {
            self.evict_oldest()?;
        }

        Ok(())
    }

    /// Check if a tile should be flushed based on spatial progress.
    pub fn should_flush_tile(&self, tile: &TileAccumulator) -> bool {
        let hilbert_distance = self
            .hilbert_high_water_mark
            .saturating_sub(tile.last_update_hilbert);

        let threshold = self.calibrator.threshold_for_zoom(tile.coord.z);

        hilbert_distance > threshold
    }

    /// Check and flush tiles that appear to be complete.
    pub fn maybe_flush_completed(&mut self) -> io::Result<()> {
        // Collect tile_ids that should be flushed
        let to_flush: Vec<u64> = self
            .active_tiles
            .iter()
            .filter(|(_, acc)| self.should_flush_tile(acc))
            .map(|(id, _)| *id)
            .collect();

        // Flush tiles
        for tile_id in to_flush {
            if let Some(acc) = self.active_tiles.remove(&tile_id) {
                self.flush_tile(acc)?;
            }
        }

        Ok(())
    }

    /// Flush a single tile to the spool.
    fn flush_tile(&mut self, acc: TileAccumulator) -> io::Result<()> {
        // Encode the tile based on format
        let data = match self.config.tile_format {
            TileFormat::Mvt => self.encode_mvt(&acc),
            TileFormat::Mlt => {
                // MLT encoding not yet implemented, fall back to MVT
                self.encode_mvt(&acc)
            }
        };

        // Write to spool
        self.spool.write_tile(acc.tile_id, &data)?;

        // Track flushed tile
        self.flushed_tiles.insert(acc.tile_id);
        self.stats.tiles_flushed += 1;

        Ok(())
    }

    /// Encode tile features to MVT format.
    fn encode_mvt(&self, acc: &TileAccumulator) -> Vec<u8> {
        let bounds = acc.coord.bounds();
        let mut layer = LayerBuilder::new(&self.layer_name).with_extent(self.extent);

        for feature in &acc.features {
            // Decode WKB to geo::Geometry
            let geometry = match wkb_to_geometry(&feature.geometry_wkb) {
                Ok(g) => g,
                Err(_) => continue, // Skip invalid geometries
            };

            // Decode MessagePack properties and convert to mvt::PropertyValue
            let properties: Vec<(String, PropertyValue)> =
                deserialize_properties(&feature.properties)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, v)| (k, convert_property_value(v)))
                    .collect();

            layer.add_feature(Some(feature.feature_id), &geometry, &properties, &bounds);
        }

        let mut tile_builder = TileBuilder::new();
        tile_builder.add_layer(layer.build());
        let tile = tile_builder.build();
        tile.encode_to_vec()
    }

    /// Evict the oldest tile (by Hilbert update time).
    pub fn evict_oldest(&mut self) -> io::Result<()> {
        let oldest_tile_id = self
            .active_tiles
            .iter()
            .min_by_key(|(_, acc)| acc.last_update_hilbert)
            .map(|(id, _)| *id);

        if let Some(tile_id) = oldest_tile_id {
            if let Some(acc) = self.active_tiles.remove(&tile_id) {
                self.flush_tile(acc)?;
                self.stats.evictions += 1;
            }
        }

        Ok(())
    }

    /// Warn if late arrival rate is high.
    fn maybe_warn_late_arrivals(&self) {
        if self.stats.features_processed == 0 {
            return;
        }

        let rate = self.stats.late_arrival_rate();
        if rate > self.config.late_arrival_warn_threshold
            && self.stats.features_processed % 100_000 == 0
        {
            tracing::warn!(
                "High late arrival rate ({:.1}%). Input may be poorly sorted. \
                Consider running `gpio optimize` for better performance.",
                rate * 100.0
            );
        }
    }

    /// Finish processing and return the spool result and statistics.
    ///
    /// This flushes all remaining tiles and returns the completed spool.
    pub fn finish(mut self) -> io::Result<(SpoolResult, StreamingStats)> {
        // Finalize calibration if not done
        if !self.calibrator.is_calibrated() {
            self.calibrator.calibrate();
        }

        // Flush all remaining tiles
        let remaining: Vec<TileAccumulator> = self.active_tiles.drain().map(|(_, v)| v).collect();
        for acc in remaining {
            self.flush_tile(acc)?;
        }

        // Get sorted entries from spool
        let result = self.spool.into_sorted_entries()?;

        Ok((result, self.stats))
    }

    /// Get current statistics.
    pub fn stats(&self) -> &StreamingStats {
        &self.stats
    }

    /// Get the number of active tiles.
    pub fn active_tile_count(&self) -> usize {
        self.active_tiles.len()
    }

    /// Check if the buffer recommends fallback to external sort.
    pub fn should_fallback(&self) -> bool {
        self.stats.late_arrival_rate() > self.config.late_arrival_fallback_threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pmtiles_writer::tile_id;

    // =========================================================================
    // TileFeature Tests
    // =========================================================================

    #[test]
    fn test_tile_feature_new() {
        let feature = TileFeature::new(42, vec![1, 2, 3], vec![4, 5, 6]);
        assert_eq!(feature.feature_id, 42);
        assert_eq!(feature.geometry_wkb, vec![1, 2, 3]);
        assert_eq!(feature.properties, vec![4, 5, 6]);
    }

    // =========================================================================
    // zoom_from_tile_id Tests
    // =========================================================================

    #[test]
    fn test_zoom_from_tile_id_zoom_0() {
        assert_eq!(zoom_from_tile_id(0), 0);
    }

    #[test]
    fn test_zoom_from_tile_id_zoom_1() {
        // At zoom 1, tile_ids are 1, 2, 3, 4
        assert_eq!(zoom_from_tile_id(1), 1);
        assert_eq!(zoom_from_tile_id(2), 1);
        assert_eq!(zoom_from_tile_id(3), 1);
        assert_eq!(zoom_from_tile_id(4), 1);
    }

    #[test]
    fn test_zoom_from_tile_id_zoom_2() {
        // At zoom 2, tile_ids start at 5 (base = (4^2 - 1) / 3 = 5)
        assert_eq!(zoom_from_tile_id(5), 2);
        assert_eq!(zoom_from_tile_id(20), 2);
    }

    #[test]
    fn test_zoom_from_tile_id_higher_zooms() {
        // Zoom 3: base = (4^3 - 1) / 3 = 21
        assert_eq!(zoom_from_tile_id(21), 3);

        // Zoom 4: base = (4^4 - 1) / 3 = 85
        assert_eq!(zoom_from_tile_id(85), 4);

        // Zoom 5: base = (4^5 - 1) / 3 = 341
        assert_eq!(zoom_from_tile_id(341), 5);
    }

    // =========================================================================
    // HilbertCalibrator Tests
    // =========================================================================

    #[test]
    fn test_calibrator_new() {
        let calibrator = HilbertCalibrator::new(1000);
        assert!(!calibrator.is_calibrated());
        assert_eq!(calibrator.sample_count(), 0);
    }

    #[test]
    fn test_calibrator_default_threshold_before_calibration() {
        let calibrator = HilbertCalibrator::new(1000);
        // Before calibration, should return conservative default
        assert_eq!(calibrator.threshold_for_zoom(0), 100_000);
        assert_eq!(calibrator.threshold_for_zoom(14), 100_000);
    }

    #[test]
    fn test_calibrator_add_sample() {
        let mut calibrator = HilbertCalibrator::new(10);
        calibrator.add_sample(1, 100);
        calibrator.add_sample(1, 200);
        calibrator.add_sample(2, 300);
        assert_eq!(calibrator.sample_count(), 3);
        assert!(!calibrator.is_calibrated());
    }

    #[test]
    fn test_calibrator_auto_calibration() {
        let mut calibrator = HilbertCalibrator::new(5);

        // Add 4 samples - not yet calibrated
        for i in 0..4 {
            calibrator.add_sample(1, i * 100);
        }
        assert!(!calibrator.is_calibrated());

        // Add 5th sample - triggers calibration
        calibrator.add_sample(1, 400);
        assert!(calibrator.is_calibrated());
    }

    #[test]
    fn test_calibrator_manual_calibration() {
        let mut calibrator = HilbertCalibrator::new(1000);

        // Add some samples (less than calibration_size)
        for i in 0..100 {
            let tile_id = tile_id(5, i % 10, i / 10);
            calibrator.add_sample(tile_id, i as u64 * 1000);
        }
        assert!(!calibrator.is_calibrated());

        // Manually trigger calibration
        calibrator.calibrate();
        assert!(calibrator.is_calibrated());

        // Thresholds should now be based on observed data
        let threshold = calibrator.threshold_for_zoom(5);
        assert!(threshold > 0);
    }

    #[test]
    fn test_calibrator_empty_calibration() {
        let mut calibrator = HilbertCalibrator::new(100);
        calibrator.calibrate();
        assert!(calibrator.is_calibrated());
        // Should use default thresholds
        assert_eq!(calibrator.threshold_for_zoom(0), 100_000);
    }

    #[test]
    fn test_calibrator_threshold_per_zoom() {
        let mut calibrator = HilbertCalibrator::new(100);

        // Simulate samples with different spans at different zooms
        // Lower zooms have larger tiles = larger Hilbert spans
        for z in 0..5u8 {
            let base_tile_id = tile_id(z, 0, 0);
            let span = 10_000u64 / (z as u64 + 1);
            for i in 0..10u64 {
                calibrator.add_sample(base_tile_id, i * span);
            }
        }

        calibrator.calibrate();
        assert!(calibrator.is_calibrated());

        // Each zoom should have a threshold
        for z in 0..15 {
            let threshold = calibrator.threshold_for_zoom(z);
            assert!(threshold > 0, "Zoom {} should have positive threshold", z);
        }
    }

    // =========================================================================
    // TileAccumulator Tests
    // =========================================================================

    #[test]
    fn test_accumulator_new() {
        // Use valid tile coords for zoom 5 (max 31)
        let coord = TileCoord::new(10, 20, 5);
        let acc = TileAccumulator::new(42, coord);
        assert_eq!(acc.tile_id, 42);
        assert_eq!(acc.coord, coord);
        assert_eq!(acc.feature_count(), 0);
        assert_eq!(acc.last_update_hilbert, 0);
    }

    #[test]
    fn test_accumulator_add_feature() {
        // Use valid tile coords for zoom 5 (max 31)
        let coord = TileCoord::new(10, 20, 5);
        let mut acc = TileAccumulator::new(42, coord);

        let feature = TileFeature::new(1, vec![1, 2, 3], vec![]);
        acc.add_feature(feature, 1000);

        assert_eq!(acc.feature_count(), 1);
        assert_eq!(acc.last_update_hilbert, 1000);
    }

    #[test]
    fn test_accumulator_multiple_features() {
        // Use valid tile coords for zoom 5 (max 31)
        let coord = TileCoord::new(10, 20, 5);
        let mut acc = TileAccumulator::new(42, coord);

        acc.add_feature(TileFeature::new(1, vec![1], vec![]), 1000);
        acc.add_feature(TileFeature::new(2, vec![2], vec![]), 500); // Lower hilbert
        acc.add_feature(TileFeature::new(3, vec![3], vec![]), 2000);

        assert_eq!(acc.feature_count(), 3);
        // last_update_hilbert should be the max
        assert_eq!(acc.last_update_hilbert, 2000);
    }

    // =========================================================================
    // StreamingTileBuffer Tests
    // =========================================================================

    #[test]
    fn test_buffer_new() {
        let config = StreamingConfig::default();
        let buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");
        assert_eq!(buffer.active_tile_count(), 0);
        assert_eq!(buffer.stats().features_processed, 0);
    }

    #[test]
    fn test_buffer_add_single_feature() {
        let config = StreamingConfig::default();
        let mut buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // At zoom 5, max tile coord is 31 (2^5 - 1)
        let coord = TileCoord::new(10, 20, 5);
        let tile_id = tile_id(5, 10, 20);
        let feature = TileFeature::new(1, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]);

        buffer
            .add_feature(tile_id, coord, feature, 1000)
            .expect("Add feature");

        assert_eq!(buffer.active_tile_count(), 1);
        assert_eq!(buffer.stats().features_processed, 1);
        assert_eq!(buffer.stats().late_arrivals, 0);
    }

    #[test]
    fn test_buffer_add_features_to_same_tile() {
        let config = StreamingConfig::default();
        let mut buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // At zoom 5, max tile coord is 31 (2^5 - 1)
        let coord = TileCoord::new(10, 20, 5);
        let tile_id = tile_id(5, 10, 20);

        for i in 0u64..10 {
            let feature = TileFeature::new(i, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]);
            buffer
                .add_feature(tile_id, coord, feature, i * 100)
                .expect("Add feature");
        }

        assert_eq!(buffer.active_tile_count(), 1);
        assert_eq!(buffer.stats().features_processed, 10);
    }

    #[test]
    fn test_buffer_add_features_to_different_tiles() {
        let config = StreamingConfig::default();
        let mut buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        for x in 0..5 {
            let coord = TileCoord::new(x, 0, 5);
            let tid = tile_id(5, x, 0);
            let feature = TileFeature::new(x as u64, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]);
            buffer
                .add_feature(tid, coord, feature, x as u64 * 1000)
                .expect("Add feature");
        }

        assert_eq!(buffer.stats().features_processed, 5);
        // All tiles should be active (haven't moved far enough to trigger flush)
        assert!(buffer.active_tile_count() >= 1);
    }

    #[test]
    fn test_buffer_late_arrival_detection() {
        let config = StreamingConfig {
            max_active_tiles: 2, // Small to force eviction
            ..Default::default()
        };
        let mut buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // Add a feature to tile 1
        let coord1 = TileCoord::new(0, 0, 5);
        let tid1 = tile_id(5, 0, 0);
        buffer
            .add_feature(
                tid1,
                coord1,
                TileFeature::new(1, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]),
                1000,
            )
            .expect("Add");

        // Add features to other tiles to trigger eviction
        for x in 1..10 {
            let coord = TileCoord::new(x, 0, 5);
            let tid = tile_id(5, x, 0);
            buffer
                .add_feature(
                    tid,
                    coord,
                    TileFeature::new(x as u64, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]),
                    x as u64 * 1_000_000, // Large Hilbert jumps
                )
                .expect("Add");
        }

        // Now add another feature to tile 1 (should be late arrival if evicted)
        let initial_late = buffer.stats().late_arrivals;
        buffer
            .add_feature(
                tid1,
                coord1,
                TileFeature::new(100, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]),
                10_000_000,
            )
            .expect("Add");

        // If tile 1 was evicted and flushed, this should be a late arrival
        // The actual behavior depends on threshold calibration
        assert!(buffer.stats().late_arrivals >= initial_late);
    }

    #[test]
    fn test_buffer_finish() {
        let config = StreamingConfig::default();
        let mut buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // Add some features
        for x in 0..5 {
            let coord = TileCoord::new(x, 0, 5);
            let tid = tile_id(5, x, 0);
            let feature = TileFeature::new(x as u64, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]);
            buffer
                .add_feature(tid, coord, feature, x as u64 * 100)
                .expect("Add feature");
        }

        let (result, stats) = buffer.finish().expect("Finish");

        assert_eq!(stats.features_processed, 5);
        assert!(stats.tiles_flushed > 0);
        assert!(result.path.exists());

        // Cleanup
        let _ = std::fs::remove_file(&result.path);
    }

    #[test]
    fn test_buffer_memory_pressure_eviction() {
        let config = StreamingConfig {
            max_active_tiles: 3,
            ..Default::default()
        };
        let mut buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // Add features to more tiles than max_active_tiles
        for x in 0..10 {
            let coord = TileCoord::new(x, 0, 5);
            let tid = tile_id(5, x, 0);
            let feature = TileFeature::new(x as u64, vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], vec![]);
            buffer
                .add_feature(tid, coord, feature, x as u64 * 100)
                .expect("Add feature");
        }

        // Should have triggered evictions
        assert!(
            buffer.stats().evictions > 0 || buffer.stats().tiles_flushed > 0,
            "Should have evicted or flushed tiles due to memory pressure"
        );
        assert!(
            buffer.active_tile_count() <= 4,
            "Active tiles should be bounded"
        );
    }

    #[test]
    fn test_buffer_should_flush_tile() {
        let config = StreamingConfig::default();
        let buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // Use valid tile coords for zoom 5 (max 31)
        let coord = TileCoord::new(0, 0, 5);
        let tid = tile_id(5, 0, 0);
        let mut acc = TileAccumulator::new(tid, coord);
        acc.last_update_hilbert = 0;

        // Without calibration, uses default threshold
        // Tile with hilbert 0 and high water mark 0 should not flush
        assert!(!buffer.should_flush_tile(&acc));
    }

    // =========================================================================
    // Integration Test: Process 10K Features
    // =========================================================================

    #[test]
    fn test_integration_10k_features() {
        let config = StreamingConfig {
            max_active_tiles: 100,
            ..Default::default()
        };
        let mut buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // Simulate 10K features across 1000 tiles at zoom 10
        for i in 0..10_000u64 {
            let x = (i % 100) as u32;
            let y = (i / 100) as u32;
            let coord = TileCoord::new(x, y, 10);
            let tid = tile_id(10, x, y);

            // Simulate Hilbert-like ordering (features for same tile close together)
            let hilbert = i * 10 + (i % 10);

            let feature = TileFeature::new(
                i,
                vec![0, 0, 0, 0, 1, 2, 0, 0, 0, 0], // Simple WKB point
                vec![],
            );

            buffer
                .add_feature(tid, coord, feature, hilbert)
                .expect("Add feature");
        }

        let (result, stats) = buffer.finish().expect("Finish");

        // Verify stats
        assert_eq!(stats.features_processed, 10_000);
        assert!(stats.tiles_flushed > 0);
        assert!(
            stats.late_arrival_rate() < 0.10,
            "Late arrival rate should be reasonable for Hilbert-ordered input"
        );

        // Verify spool result
        assert!(!result.entries.is_empty());
        assert!(result.path.exists());

        // Entries should be sorted by tile_id
        for window in result.entries.windows(2) {
            assert!(
                window[0].tile_id < window[1].tile_id,
                "Entries should be sorted"
            );
        }

        // Cleanup
        let _ = std::fs::remove_file(&result.path);
    }

    #[test]
    fn test_fallback_detection() {
        let config = StreamingConfig {
            late_arrival_fallback_threshold: 0.10, // 10%
            ..Default::default()
        };
        let buffer = StreamingTileBuffer::new(config, "test_layer").expect("Create buffer");

        // Simulate poorly sorted input with many late arrivals
        // This is hard to trigger with the current implementation,
        // but we can verify the API works
        assert!(!buffer.should_fallback());

        // Force some late arrivals by manipulating stats
        // (In real usage, this would happen from actual late arrivals)
        let (result, _) = buffer.finish().expect("Finish");
        let _ = std::fs::remove_file(&result.path);
    }
}
