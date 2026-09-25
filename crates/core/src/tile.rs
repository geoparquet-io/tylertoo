//! Tile coordinate math and utilities
//!
//! This module provides functions for converting between geographic coordinates (lat/lng)
//! and tile coordinates (x/y/z) using Web Mercator projection.

use std::f64::consts::PI;

use crate::world_coord::MAX_LATITUDE;

/// The maximum Web Mercator zoom tylertoo will **write**.
///
/// Three limits stack up, and 30 is the largest zoom that clears all of them
/// with room to spare (#371):
///
/// * **Tile coordinates are `u32`.** A tile index at zoom `z` runs to
///   `2^z - 1`, so `z <= 31` is the hard ceiling and `z == 32` wraps to 0.
/// * **The PMTiles Hilbert tile id is `u64`.** Its base term is
///   `sum(4^i for i in 1..z)`, which needs `4^z` headroom — `4u64.pow` blows
///   past `u64` at z32, and every `1u32 << z` in the Hilbert transform masks
///   to `n = 1` in release rather than panicking, silently corrupting *every*
///   tile id in the archive.
/// * **The grid is already absurd.** z30 is ~1.15e18 tiles and a ground
///   sample distance of ~3.6e-5 m; no real dataset resolves past it.
///
/// So: take what the math supports (z31), keep one level of safety margin,
/// and land on 30 — the value [`crate::pyramid::Band`] has always enforced,
/// now shared by the conversion and export validators so every write path
/// rejects an out-of-range zoom *before* doing any work.
///
/// This is deliberately the **write/convert-side** cap. Reading a foreign
/// PMTiles archive is a separate question (an archive may legitimately
/// address up to z31); see [`crate::pmtiles_writer::tile_id_to_zxy`].
pub const MAX_ZOOM: u8 = 30;

/// Tiles per axis at `zoom` (`2^zoom`), computed in `u64` and clamped at
/// `2^32`.
///
/// `2u32.pow(zoom)` / `1u32 << zoom` panic in debug and silently mask in
/// release once `zoom >= 32` (#371). The clamp is a **policy bound, not a
/// true tile count**: `2^32` is the largest grid a 32-bit tile coordinate can
/// index (`0..=u32::MAX`), so it is where this crate stops counting — past
/// z32 the value returned is that bound, not `2^zoom`.
///
/// Every caller is validated against [`MAX_ZOOM`] long before it gets here, so
/// the clamp is unreachable in practice — it exists so the paths that bypass
/// options validation (PMTiles reading, hand-built [`TileCoord`]s) degrade to
/// a clamped value instead of a wrapped one.
#[inline]
pub fn tiles_per_axis(zoom: u8) -> u64 {
    1u64 << zoom.min(32)
}

/// Highest valid tile index on either axis at `zoom` (`2^zoom - 1`),
/// saturating at [`u32::MAX`]. See [`tiles_per_axis`] (#371).
#[inline]
pub fn max_tile_index(zoom: u8) -> u32 {
    (tiles_per_axis(zoom) - 1).min(u64::from(u32::MAX)) as u32
}

/// Tile coordinates: x, y, and zoom level
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileCoord {
    pub x: u32,
    pub y: u32,
    pub z: u8,
}

impl TileCoord {
    /// Create a new tile coordinate
    pub fn new(x: u32, y: u32, z: u8) -> Self {
        Self { x, y, z }
    }

    /// Get the bounding box of this tile in geographic coordinates (lng/lat)
    pub fn bounds(&self) -> TileBounds {
        let n = 2_f64.powi(self.z as i32);
        let lng_min = (self.x as f64) / n * 360.0 - 180.0;
        let lng_max = (self.x as f64 + 1.0) / n * 360.0 - 180.0;

        let lat_rad = |y: f64| {
            let y_rad = PI * (1.0 - 2.0 * y / n);
            y_rad.sinh().atan().to_degrees()
        };

        let lat_max = lat_rad(self.y as f64);
        let lat_min = lat_rad(self.y as f64 + 1.0);

        TileBounds {
            lng_min,
            lat_min,
            lng_max,
            lat_max,
        }
    }

    /// Get the parent tile at zoom level z-1.
    ///
    /// Each tile at zoom z has exactly one parent at zoom z-1.
    /// The parent contains this tile and its 3 siblings (2x2 grid).
    /// Returns `None` at zoom 0 (no parent).
    pub fn parent(&self) -> Option<TileCoord> {
        if self.z == 0 {
            return None;
        }
        Some(TileCoord::new(self.x / 2, self.y / 2, self.z - 1))
    }

    /// Get the four child tiles at zoom level z+1.
    ///
    /// Each tile at zoom z has exactly four children at zoom z+1,
    /// forming a 2x2 grid that exactly covers the parent tile.
    /// Returns `None` at [`MAX_ZOOM`] (the maximum supported zoom).
    pub fn children(&self) -> Option<[TileCoord; 4]> {
        if self.z >= MAX_ZOOM {
            return None;
        }
        let child_z = self.z + 1;
        let cx = self.x * 2;
        let cy = self.y * 2;
        Some([
            TileCoord::new(cx, cy, child_z),         // top-left
            TileCoord::new(cx + 1, cy, child_z),     // top-right
            TileCoord::new(cx, cy + 1, child_z),     // bottom-left
            TileCoord::new(cx + 1, cy + 1, child_z), // bottom-right
        ])
    }
}

/// Geographic bounding box
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileBounds {
    pub lng_min: f64,
    pub lat_min: f64,
    pub lng_max: f64,
    pub lat_max: f64,
}

impl TileBounds {
    /// Create a new bounding box
    pub fn new(lng_min: f64, lat_min: f64, lng_max: f64, lat_max: f64) -> Self {
        Self {
            lng_min,
            lat_min,
            lng_max,
            lat_max,
        }
    }

    /// Create an empty/invalid bounding box
    pub fn empty() -> Self {
        Self {
            lng_min: f64::INFINITY,
            lat_min: f64::INFINITY,
            lng_max: f64::NEG_INFINITY,
            lat_max: f64::NEG_INFINITY,
        }
    }

    /// Check if this is a valid bounding box
    pub fn is_valid(&self) -> bool {
        self.lng_min <= self.lng_max && self.lat_min <= self.lat_max
    }

    /// Expand this bounding box to include another
    pub fn expand(&mut self, other: &Self) {
        self.lng_min = self.lng_min.min(other.lng_min);
        self.lat_min = self.lat_min.min(other.lat_min);
        self.lng_max = self.lng_max.max(other.lng_max);
        self.lat_max = self.lat_max.max(other.lat_max);
    }

    /// The overlap of two bounding boxes.
    ///
    /// The dual of [`TileBounds::expand`], and no more clever: it can return
    /// an *invalid* box (one whose min is past its max) when the two do not
    /// overlap, which [`TileBounds::is_valid`] reports. Callers that narrow a
    /// box they already know overlaps — a shard clipping a level's extent to
    /// its own range (#498) — want exactly this.
    pub fn intersect(&self, other: &Self) -> Self {
        Self {
            lng_min: self.lng_min.max(other.lng_min),
            lat_min: self.lat_min.max(other.lat_min),
            lng_max: self.lng_max.min(other.lng_max),
            lat_max: self.lat_max.min(other.lat_max),
        }
    }

    /// Get the width in degrees
    pub fn width(&self) -> f64 {
        self.lng_max - self.lng_min
    }

    /// Get the height in degrees
    pub fn height(&self) -> f64 {
        self.lat_max - self.lat_min
    }
}

/// Convert longitude/latitude to tile coordinates at a given zoom level
///
/// Uses Web Mercator projection (EPSG:3857)
///
/// # Arguments
///
/// * `lng` - Longitude in degrees (-180 to 180)
/// * `lat` - Latitude in degrees. Values outside the Web Mercator bounds
///   (`±world_coord::MAX_LATITUDE`) are silently clamped, not rejected.
/// * `zoom` - Zoom level (0-30)
///
/// # Returns
///
/// TileCoord with x, y, and zoom
pub fn lng_lat_to_tile(lng: f64, lat: f64, zoom: u8) -> TileCoord {
    let n = 2_f64.powi(zoom as i32);

    // Maximum valid tile coordinate at this zoom level.
    // #371: via `max_tile_index`, which is u64 internally — `2u32.pow(zoom)`
    // overflowed at z32 (debug panic, release mask).
    let max_coord = max_tile_index(zoom);

    // Convert longitude to tile x
    // Clamp to valid range to handle lng=180° edge case (which would produce x=2^z)
    let x = ((lng + 180.0) / 360.0 * n).floor() as u32;
    let x = x.min(max_coord);

    // Clamp latitude to the Web Mercator bound to prevent tile coordinate
    // overflow for out-of-range latitudes. ±MAX_LATITUDE is not where the
    // projection stops being defined — it is where its y reaches the edge of
    // the square world extent (normalized y = 0 north, 1 south). The
    // projection itself only diverges at ±90°, which is why without this
    // clamp lat=-90° produces y values 6-20x larger than valid bounds.
    //
    // The bound is used exactly, not shaved to a rounder value: clamping
    // short of it silently moved points that legitimately sit at the edge of
    // a full EPSG:3857 extent into the wrong tile row (#416).
    let lat = lat.clamp(-MAX_LATITUDE, MAX_LATITUDE);

    // Convert latitude to tile y (Web Mercator)
    let lat_rad = lat.to_radians();
    let y = ((1.0 - lat_rad.tan().asinh() / PI) / 2.0 * n).floor() as u32;
    // Defense in depth for the south edge: at lat = -MAX_LATITUDE the raw row
    // sits infinitesimally below 2^z, and a rounding error in the other
    // direction would floor it to exactly 2^z — one past the last valid row.
    // With MAX_LATITUDE rounded down this no longer triggers, but the clamp
    // still guards any future change to the constant's last digits.
    let y = y.min(max_coord);

    TileCoord::new(x, y, zoom)
}

/// Get the geographic bounds of a tile
///
/// Convenience function that wraps `TileCoord::bounds()`
pub fn tile_bounds(x: u32, y: u32, z: u8) -> TileBounds {
    TileCoord::new(x, y, z).bounds()
}

/// The first PMTiles tile id at zoom `z`: `(4^z - 1) / 3`, the count of tiles
/// at every shallower zoom (`sum(4^i for i in 0..z)`).
///
/// This is the same cumulative base [`crate::pmtiles_writer::tile_id`] adds
/// its within-zoom Hilbert index to (that function's `base_id` is this value
/// minus 1, folded into a `+ hilbert_idx + 1` for `z >= 1`; both forms agree
/// for every `z`, this one included at `z = 0`).
#[inline]
pub(crate) fn hilbert_zoom_base(z: u8) -> u64 {
    ((1u64 << (2 * u32::from(z))) - 1) / 3
}

/// The inclusive range of PMTiles tile ids, at `target_z`, addressed by
/// `node`'s subtree — every descendant leaf of `node` down to `target_z`.
///
/// # The nesting property
///
/// A PMTiles tile id is `base(z) + hilbert_idx(z, x, y)`, where
/// `base(z) = (4^z - 1) / 3` is the count of tiles at shallower zooms and
/// `hilbert_idx` is the tile's position on the zoom's Hilbert curve. The curve
/// is built by recursive quadrant subdivision: the walk enters a node, covers
/// each of its four children completely before moving to the next, and leaves.
/// Each child's own sub-curve is rotated and/or reflected relative to the
/// parent's — that is what makes it a Hilbert curve rather than a Z-order
/// curve — but the rotation only permutes the order *within* a child's
/// quarter; the walk never leaves the quarter mid-child. Bit-wise, the
/// consequence is a fixed prefix: a depth-`z` Hilbert index's leading
/// `2 * zn` bits are exactly the depth-`zn` Hilbert index of that tile's
/// zoom-`zn` ancestor, whatever the rotations below. So a node at zoom `zn`
/// with Hilbert index `h` owns exactly the descendant ids
///
/// ```text
/// base(z) + h * 4^Δ  ..=  base(z) + (h + 1) * 4^Δ - 1,   Δ = z - zn
/// ```
///
/// at any deeper zoom `z` — the prefix pinned to `h` while the low `2Δ` bits
/// range over all of `0 ..= 4^Δ - 1`, i.e. a single contiguous interval, not
/// merely a superset. That is what lets the export cascade prune a subtree against a
/// partition's `[key_lo, key_hi]` window with an *exact* interval
/// intersection instead of the old conservative row-major bounding check.
///
/// Crate-private: the export cascade (`overview::export::node_key_overlaps`)
/// is the only caller, and it always passes a node produced by descending
/// from the cascade root towards `target_z = zoom`, so `node.z <= target_z`
/// holds by construction there. A hand-built call from outside the crate
/// would have no such guarantee, and there is no meaningful "descendant
/// range" to return when it does not hold — see `# Panics` below — so this
/// stays `pub(crate)` rather than a public API a caller could misuse.
///
/// # Panics
///
/// Debug-asserts `target_z <= `[`MAX_TILE_ID_ZOOM`][crate::pmtiles_writer::MAX_TILE_ID_ZOOM]
/// and `target_z >= node.z`. Both guard a real failure mode, not a paranoia
/// check: past `MAX_TILE_ID_ZOOM` (31), `1u64 << (2 * delta)` below shifts by
/// 64 or more bits, which panics in debug and — the actual danger — silently
/// masks to a near-zero shift in release (#371's `xy_to_hilbert` hits the
/// identical failure mode one function over). `target_z < node.z` has no
/// valid answer at all: `node.z`'s own Hilbert index and `target_z`'s
/// cumulative base are then different zooms' incompatible units, and
/// `saturating_sub` would silently swallow that into `delta = 0` rather than
/// surface it. In a release build (assertions compiled out) `target_z` is
/// additionally clamped to `MAX_TILE_ID_ZOOM` before the shift, so an
/// out-of-range call degrades to a wrong-but-bounded answer instead of the
/// masked-shift garbage a raw `1u64 << 64+` would produce.
pub(crate) fn node_id_range(node: TileCoord, target_z: u8) -> std::ops::RangeInclusive<u64> {
    debug_assert!(
        target_z <= crate::pmtiles_writer::MAX_TILE_ID_ZOOM,
        "node_id_range: target_z ({target_z}) exceeds the deepest zoom a PMTiles tile id \
         can address ({})",
        crate::pmtiles_writer::MAX_TILE_ID_ZOOM
    );
    debug_assert!(
        target_z >= node.z,
        "node_id_range: target_z ({target_z}) must be >= node.z ({})",
        node.z
    );
    // See `# Panics`: unreachable when the two asserts above hold, kept as
    // this function's own guard rail so a release build stays total instead
    // of computing a masked, essentially-random shift.
    let target_z = target_z.min(crate::pmtiles_writer::MAX_TILE_ID_ZOOM);
    let delta = target_z.saturating_sub(node.z);
    let h = crate::pmtiles_writer::xy_to_hilbert(node.z, node.x, node.y);
    let base_target = hilbert_zoom_base(target_z);
    // 4^delta descendant leaves per node at this depth; exact in u64 for every
    // delta this crate ever sees (target_z <= MAX_TILE_ID_ZOOM = 31).
    let span = 1u64 << (2 * u32::from(delta));
    let start = base_target + h * span;
    let end = start + span - 1;
    start..=end
}

/// Get all tiles that intersect a geographic bounding box at a given zoom level
///
/// Handles antimeridian crossing: when `lng_min > lng_max`, the bbox crosses
/// the antimeridian (180° longitude) and is split into two ranges:
/// `[lng_min, 180°]` and `[-180°, lng_max]`.
///
/// # Arguments
///
/// * `bbox` - Geographic bounding box
/// * `zoom` - Zoom level
///
/// # Returns
///
/// Iterator of TileCoord that intersect the bbox
pub fn tiles_for_bbox(bbox: &TileBounds, zoom: u8) -> impl Iterator<Item = TileCoord> {
    let r = tile_ranges_for_bbox(bbox, zoom);
    let (min_y_tile, max_y_tile) = r.y;
    let first = r.x;
    let second = r.x2;

    // Generate tiles for the first x-range
    let first_tiles = (min_y_tile..=max_y_tile)
        .flat_map(move |y| (first.0..=first.1).map(move |x| TileCoord::new(x, y, zoom)));

    // Generate tiles for the second x-range (if crossing antimeridian)
    let second_tiles = second.into_iter().flat_map(move |(x_min, x_max)| {
        (min_y_tile..=max_y_tile)
            .flat_map(move |y| (x_min..=x_max).map(move |x| TileCoord::new(x, y, zoom)))
    });

    first_tiles.chain(second_tiles)
}

/// Inclusive tile-index ranges covering a bbox at a zoom: one y-range and one
/// or two x-ranges (two when the bbox crosses the antimeridian).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BboxTileRanges {
    /// Inclusive `(min_y, max_y)` tile range.
    pub y: (u32, u32),
    /// Primary inclusive `(min_x, max_x)` tile range.
    pub x: (u32, u32),
    /// Second `(min_x, max_x)` range, present only for antimeridian-crossing
    /// bboxes (`[lng_min, 180°]` = `x`, `[-180°, lng_max]` = `x2`).
    pub x2: Option<(u32, u32)>,
}

/// Compute the antimeridian-aware tile-index ranges of `bbox` at `zoom`.
///
/// This is the shared authority for "which tiles a bbox covers": both
/// [`tiles_for_bbox`] and the export recursive tile splitter drive off it, so
/// their leaf-tile sets are identical by construction (no lost or extra tiles).
pub(crate) fn tile_ranges_for_bbox(bbox: &TileBounds, zoom: u8) -> BboxTileRanges {
    let crosses_antimeridian = bbox.lng_min > bbox.lng_max;

    // Get y-tile range (latitude doesn't wrap)
    let min_y_tile = lng_lat_to_tile(bbox.lng_min, bbox.lat_max, zoom).y; // lat_max -> min_y
    let max_y_tile = lng_lat_to_tile(bbox.lng_min, bbox.lat_min, zoom).y; // lat_min -> max_y

    // #371: u64 tile-grid math, saturating instead of overflowing at z32.
    let max_tile_x = max_tile_index(zoom);

    // Calculate x-tile ranges
    let (x, x2): ((u32, u32), Option<(u32, u32)>) = if crosses_antimeridian {
        // Split into two ranges: [lng_min, 180°] and [-180°, lng_max]
        let west_x = lng_lat_to_tile(bbox.lng_min, 0.0, zoom).x; // From lng_min to 180°
        let east_x = lng_lat_to_tile(bbox.lng_max, 0.0, zoom).x; // From -180° to lng_max

        // First range: lng_min to 180° (west_x to max_tile_x)
        // Second range: -180° to lng_max (0 to east_x)
        ((west_x, max_tile_x), Some((0, east_x)))
    } else {
        // Normal case: single range
        let min_x = lng_lat_to_tile(bbox.lng_min, 0.0, zoom).x;
        let max_x = lng_lat_to_tile(bbox.lng_max, 0.0, zoom).x;
        ((min_x, max_x), None)
    };

    BboxTileRanges {
        y: (min_y_tile, max_y_tile),
        x,
        x2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lng_lat_to_tile_origin() {
        // Origin (null island: 0, 0) at zoom 0
        let tile = lng_lat_to_tile(0.0, 0.0, 0);
        assert_eq!(tile, TileCoord::new(0, 0, 0));
    }

    #[test]
    fn test_lng_lat_to_tile_zoom_1() {
        // Test various points at zoom 1
        let tile = lng_lat_to_tile(0.0, 0.0, 1);
        assert_eq!(tile.x, 1);
        assert_eq!(tile.y, 1);
        assert_eq!(tile.z, 1);

        // Top-left quadrant
        let tile = lng_lat_to_tile(-90.0, 45.0, 1);
        assert_eq!(tile.x, 0);

        // Top-right quadrant
        let tile = lng_lat_to_tile(90.0, 45.0, 1);
        assert_eq!(tile.x, 1);
    }

    #[test]
    fn test_tile_bounds() {
        // Tile 0,0,0 should cover the whole world
        let tile = TileCoord::new(0, 0, 0);
        let bounds = tile.bounds();

        assert!((bounds.lng_min - (-180.0)).abs() < 0.0001);
        assert!((bounds.lng_max - 180.0).abs() < 0.0001);
        // Lat bounds are Web Mercator limits (~85.05 degrees)
        assert!(bounds.lat_min < -85.0);
        assert!(bounds.lat_max > 85.0);
    }

    #[test]
    fn test_tiles_for_bbox_single_tile() {
        // Small bbox that fits in one tile
        let bbox = TileBounds::new(-1.0, -1.0, 1.0, 1.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 10).collect();

        // Should be at least 1 tile
        assert!(!tiles.is_empty());

        // All tiles should be at zoom 10
        for tile in &tiles {
            assert_eq!(tile.z, 10);
        }
    }

    #[test]
    fn test_tiles_for_bbox_multiple_tiles() {
        // Larger bbox spanning multiple tiles at zoom 5
        let bbox = TileBounds::new(-10.0, -10.0, 10.0, 10.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 5).collect();

        // Should cover multiple tiles
        assert!(tiles.len() > 1);

        // Check bounds are reasonable
        let first = tiles.first().unwrap();
        let last = tiles.last().unwrap();
        assert!(first.x <= last.x);
        assert!(first.y <= last.y);
    }

    #[test]
    fn test_bbox_expand() {
        let mut bbox1 = TileBounds::new(-10.0, -10.0, 10.0, 10.0);
        let bbox2 = TileBounds::new(-20.0, -5.0, 5.0, 15.0);

        bbox1.expand(&bbox2);

        assert_eq!(bbox1.lng_min, -20.0);
        assert_eq!(bbox1.lat_min, -10.0);
        assert_eq!(bbox1.lng_max, 10.0);
        assert_eq!(bbox1.lat_max, 15.0);
    }

    #[test]
    fn test_bbox_empty() {
        let bbox = TileBounds::empty();
        assert!(!bbox.is_valid());

        let mut bbox = TileBounds::empty();
        bbox.expand(&TileBounds::new(-10.0, -10.0, 10.0, 10.0));
        assert!(bbox.is_valid());
        assert_eq!(bbox.lng_min, -10.0);
    }

    /// #371 boundary: the grid helpers are exact at the write ceiling and, past
    /// the 32-bit coordinate limit, return the clamp rather than a wrapped
    /// value. `2u32.pow(zoom)` panicked in debug and wrapped to 0 in release
    /// at z32.
    #[test]
    fn tile_grid_helpers_are_exact_at_max_zoom_and_clamp_beyond() {
        assert_eq!(MAX_ZOOM, 30);
        assert_eq!(tiles_per_axis(0), 1);
        assert_eq!(tiles_per_axis(MAX_ZOOM), 1 << 30);
        assert_eq!(tiles_per_axis(MAX_ZOOM), 1_073_741_824);
        assert_eq!(max_tile_index(MAX_ZOOM), 1_073_741_823);
        // z31 and z32 are both still exact — 2^32 - 1 is u32::MAX, the last
        // index the coordinate type can name — where `2u32.pow(32)` wrapped
        // to 0. The clamp only starts standing in for the true count at z33.
        assert_eq!(max_tile_index(31), u32::MAX / 2);
        assert_eq!(max_tile_index(32), u32::MAX);
        for z in [33u8, 64, 255] {
            assert_eq!(max_tile_index(z), u32::MAX, "z{z} must clamp, not wrap");
        }
    }

    /// #371: tile lookup at the ceiling stays inside the grid. Before the fix
    /// this whole family of call sites shared one `2u32.pow(zoom)`.
    #[test]
    fn lng_lat_to_tile_stays_in_range_at_max_zoom() {
        let max = max_tile_index(MAX_ZOOM);
        for (lng, lat) in [
            (-180.0, 85.05),
            (180.0, -85.05),
            (0.0, 0.0),
            (179.999, 84.9),
        ] {
            let t = lng_lat_to_tile(lng, lat, MAX_ZOOM);
            assert_eq!(t.z, MAX_ZOOM);
            assert!(t.x <= max, "x {} out of range at z{MAX_ZOOM}", t.x);
            assert!(t.y <= max, "y {} out of range at z{MAX_ZOOM}", t.y);
        }
        // The whole world at z30 spans the whole grid.
        let world = TileBounds::new(-180.0, -85.05, 180.0, 85.05);
        let ranges = tile_ranges_for_bbox(&world, MAX_ZOOM);
        assert_eq!(ranges.x, (0, max));
        assert!(ranges.x2.is_none());
    }

    /// `children()` is the pyramid walk's stopping rule; it must stop at the
    /// shared ceiling rather than a hard-coded literal (#371).
    #[test]
    fn children_stop_at_max_zoom() {
        assert!(TileCoord::new(0, 0, MAX_ZOOM - 1).children().is_some());
        assert!(TileCoord::new(0, 0, MAX_ZOOM).children().is_none());
    }

    #[test]
    fn test_tile_coord_round_trip() {
        // For various zooms, check that a tile's center converts back to the same tile
        for zoom in 0..=14 {
            // Use valid tile coordinates for each zoom (max tile = 2^zoom - 1)
            let max_coord = max_tile_index(zoom);
            let x = max_coord.min(100);
            let y = max_coord.min(200);

            let tile = TileCoord::new(x, y, zoom);
            let bounds = tile.bounds();

            let center_lng = (bounds.lng_min + bounds.lng_max) / 2.0;
            let center_lat = (bounds.lat_min + bounds.lat_max) / 2.0;

            let tile_back = lng_lat_to_tile(center_lng, center_lat, zoom);

            assert_eq!(tile, tile_back, "Round-trip failed at zoom {}", zoom);
        }
    }

    #[test]
    fn test_tiles_for_bbox_antimeridian_crossing() {
        // Fiji area: bbox from 170°E to 170°W (which is -170°)
        // This crosses the antimeridian at 180°
        let bbox = TileBounds::new(170.0, -20.0, -170.0, -10.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 4).collect();

        // Should NOT be empty - this is the bug we're fixing
        assert!(
            !tiles.is_empty(),
            "Antimeridian crossing bbox should produce tiles"
        );

        // Collect all unique x coordinates
        let x_coords: std::collections::HashSet<_> = tiles.iter().map(|t| t.x).collect();

        // At zoom 4, the world is 16 tiles wide (0-15)
        // 170° is around x=15, -170° is around x=0
        // We should have tiles on BOTH sides of the antimeridian
        let has_high_x = x_coords.iter().any(|&x| x >= 15); // Near 180° (east side)
        let has_low_x = x_coords.iter().any(|&x| x <= 1); // Near -180° (west side)

        assert!(
            has_high_x && has_low_x,
            "Should have tiles on both sides of antimeridian. Got x coords: {:?}",
            x_coords
        );
    }

    #[test]
    fn antimeridian_inflated_bbox_covers_full_world_row() {
        // Issue #188 behavior pin. `tiles_for_bbox` supports wrapped bboxes
        // (lng_min > lng_max, tested above), but the overview pipeline never
        // produces one: bboxes come from `geo::bounding_rect` (plain min/max),
        // so an antimeridian-crossing feature arrives as the INFLATED bbox
        // [-179.9, .., 179.9]. That bbox enumerates every x column at the
        // zoom — the full world row — not two columns at ±180°.
        // See `context/ANTIMERIDIAN.md`.
        let bbox = TileBounds::new(-179.9, -0.1, 179.9, 0.1);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 4).collect();
        let x_coords: std::collections::HashSet<_> = tiles.iter().map(|t| t.x).collect();
        assert_eq!(
            x_coords.len(),
            16,
            "PIN: inflated antimeridian bbox spans all 16 x columns at z4"
        );
    }

    #[test]
    fn test_tiles_for_bbox_normal_still_works() {
        // Normal case: Europe (doesn't cross antimeridian)
        let bbox = TileBounds::new(-10.0, 40.0, 10.0, 50.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 4).collect();

        assert!(!tiles.is_empty(), "Normal bbox should produce tiles");

        // All tiles should be in the expected range
        for tile in &tiles {
            assert_eq!(tile.z, 4);
        }
    }

    // ========== Parent/Children Tests ==========

    #[test]
    fn test_tile_parent_at_zoom_0() {
        let tile = TileCoord::new(0, 0, 0);
        assert_eq!(tile.parent(), None, "Zoom 0 tile has no parent");
    }

    #[test]
    fn test_tile_parent_at_zoom_1() {
        // All four z=1 tiles should have z=0/0/0 as parent
        for x in 0..2 {
            for y in 0..2 {
                let tile = TileCoord::new(x, y, 1);
                let parent = tile.parent().expect("z=1 tile should have parent");
                assert_eq!(parent, TileCoord::new(0, 0, 0));
            }
        }
    }

    #[test]
    fn test_tile_parent_at_higher_zoom() {
        let tile = TileCoord::new(5, 7, 4);
        let parent = tile.parent().expect("Should have parent");
        assert_eq!(parent, TileCoord::new(2, 3, 3));

        let grandparent = parent.parent().expect("Should have grandparent");
        assert_eq!(grandparent, TileCoord::new(1, 1, 2));
    }

    #[test]
    fn test_tile_children() {
        let tile = TileCoord::new(1, 2, 3);
        let children = tile.children().expect("Should have children");

        assert_eq!(children[0], TileCoord::new(2, 4, 4)); // top-left
        assert_eq!(children[1], TileCoord::new(3, 4, 4)); // top-right
        assert_eq!(children[2], TileCoord::new(2, 5, 4)); // bottom-left
        assert_eq!(children[3], TileCoord::new(3, 5, 4)); // bottom-right
    }

    #[test]
    fn test_tile_children_at_max_zoom() {
        let tile = TileCoord::new(0, 0, 30);
        assert_eq!(tile.children(), None, "Zoom 30 tile has no children");
    }

    #[test]
    fn test_parent_child_round_trip() {
        // A tile's parent's children should include the original tile
        let tile = TileCoord::new(5, 7, 4);
        let parent = tile.parent().unwrap();
        let siblings = parent.children().unwrap();
        assert!(
            siblings.contains(&tile),
            "Parent's children should include original tile"
        );
    }

    #[test]
    fn test_child_parent_round_trip() {
        // Each child's parent should be the original tile
        let tile = TileCoord::new(3, 2, 5);
        let children = tile.children().unwrap();
        for child in &children {
            assert_eq!(
                child.parent().unwrap(),
                tile,
                "Each child's parent should be the original tile"
            );
        }
    }

    #[test]
    fn test_children_cover_parent_bounds() {
        // The four children should collectively cover the parent's bounds
        let parent = TileCoord::new(1, 1, 2);
        let parent_bounds = parent.bounds();
        let children = parent.children().unwrap();

        // Find the bounding box of all children
        let mut min_lng = f64::INFINITY;
        let mut max_lng = f64::NEG_INFINITY;
        let mut min_lat = f64::INFINITY;
        let mut max_lat = f64::NEG_INFINITY;

        for child in &children {
            let b = child.bounds();
            min_lng = min_lng.min(b.lng_min);
            max_lng = max_lng.max(b.lng_max);
            min_lat = min_lat.min(b.lat_min);
            max_lat = max_lat.max(b.lat_max);
        }

        assert!(
            (min_lng - parent_bounds.lng_min).abs() < 1e-10,
            "Children lng_min should match parent"
        );
        assert!(
            (max_lng - parent_bounds.lng_max).abs() < 1e-10,
            "Children lng_max should match parent"
        );
        assert!(
            (min_lat - parent_bounds.lat_min).abs() < 1e-10,
            "Children lat_min should match parent"
        );
        assert!(
            (max_lat - parent_bounds.lat_max).abs() < 1e-10,
            "Children lat_max should match parent"
        );
    }

    #[test]
    fn test_tiles_for_bbox_antimeridian_tile_count() {
        // At zoom 2, tiles are ~90° wide
        // A bbox from 170° to -170° spans about 20° (across the antimeridian)
        // Should produce a reasonable number of tiles, not wrap around the whole world
        let bbox = TileBounds::new(170.0, -20.0, -170.0, -10.0);
        let tiles: Vec<_> = tiles_for_bbox(&bbox, 2).collect();

        // At zoom 2 (4x4 grid), this should be ~1-2 tiles in x direction
        // The bbox is small, just crossing the antimeridian
        let x_coords: std::collections::HashSet<_> = tiles.iter().map(|t| t.x).collect();

        // Should have tiles from x=3 (170°-180°) and x=0 (-180° to -170°)
        assert!(
            x_coords.len() <= 3,
            "Antimeridian bbox should produce tiles only near the crossing, not wrap around. Got {} unique x coords: {:?}",
            x_coords.len(),
            x_coords
        );
    }

    #[test]
    fn test_lng_lat_to_tile_boundary_clamping() {
        // Test that lng=180 and lat=-85.05 don't produce out-of-bounds tile coordinates
        // This was a bug: lng=180 at zoom 0 produced x=1, but only x=0 is valid at zoom 0

        // At zoom 0, only tile (0,0,0) exists
        let tile = lng_lat_to_tile(180.0, 0.0, 0);
        assert_eq!(tile.x, 0, "lng=180 at zoom 0 should clamp to x=0");
        assert_eq!(tile.y, 0, "lat=0 at zoom 0 should be y=0");

        let tile = lng_lat_to_tile(180.0, -85.05, 0);
        assert_eq!(tile.x, 0, "lng=180 at zoom 0 should clamp to x=0");
        assert_eq!(tile.y, 0, "lat=-85.05 at zoom 0 should clamp to y=0");

        // At zoom 1, only x in [0,1] and y in [0,1] are valid
        let tile = lng_lat_to_tile(180.0, 0.0, 1);
        assert!(tile.x <= 1, "lng=180 at zoom 1 should have x <= 1");

        // Test various edge cases
        for zoom in 0..=10 {
            let max_valid = max_tile_index(zoom);

            let tile_pos180 = lng_lat_to_tile(180.0, 0.0, zoom);
            assert!(
                tile_pos180.x <= max_valid,
                "lng=180 at zoom {} should have x <= {}, got {}",
                zoom,
                max_valid,
                tile_pos180.x
            );

            let tile_neg180 = lng_lat_to_tile(-180.0, 0.0, zoom);
            assert_eq!(
                tile_neg180.x, 0,
                "lng=-180 at zoom {} should have x = 0",
                zoom
            );

            let tile_north_pole = lng_lat_to_tile(0.0, 85.05, zoom);
            assert!(
                tile_north_pole.y <= max_valid,
                "lat=85.05 at zoom {} should have y <= {}",
                zoom,
                max_valid
            );

            let tile_south_pole = lng_lat_to_tile(0.0, -85.05, zoom);
            assert!(
                tile_south_pole.y <= max_valid,
                "lat=-85.05 at zoom {} should have y <= {}",
                zoom,
                max_valid
            );
        }
    }

    /// [`node_id_range`] must match a brute-force enumeration of every
    /// descendant tile id, exhaustively for all nodes at `z <= 4` and every
    /// deeper target zoom up to `z + 2`: the returned interval's endpoints
    /// equal the min/max of the descendant set, AND every id in between is
    /// actually a descendant (no gaps) -- the nesting property this PR's
    /// tile-id-ordered export leans on.
    #[test]
    fn node_id_range_matches_bruteforce_descendants() {
        for z in 0u8..=4 {
            let n = 1u32 << z;
            for x in 0..n {
                for y in 0..n {
                    let node = TileCoord::new(x, y, z);
                    for delta in 0u8..=2 {
                        let target_z = z + delta;
                        let shift = u32::from(delta);
                        let x0 = x << shift;
                        let y0 = y << shift;
                        let span = 1u32 << shift;
                        let mut ids: Vec<u64> = Vec::with_capacity((span * span) as usize);
                        for dx in 0..span {
                            for dy in 0..span {
                                ids.push(crate::pmtiles_writer::tile_id(
                                    target_z,
                                    x0 + dx,
                                    y0 + dy,
                                ));
                            }
                        }
                        ids.sort_unstable();

                        let range = node_id_range(node, target_z);
                        assert_eq!(
                            *range.start(),
                            ids[0],
                            "node ({x},{y},{z}) -> z{target_z}: range start diverges"
                        );
                        assert_eq!(
                            *range.end(),
                            *ids.last().unwrap(),
                            "node ({x},{y},{z}) -> z{target_z}: range end diverges"
                        );
                        // No gaps: the brute-force set, sorted, must be exactly
                        // the contiguous run [start, end] -- not just share its
                        // endpoints.
                        let expected: Vec<u64> = range.clone().collect();
                        assert_eq!(
                            ids, expected,
                            "node ({x},{y},{z}) -> z{target_z}: descendant ids are not \
                             the contiguous interval node_id_range claims"
                        );
                    }
                }
            }
        }
    }

    /// #506 review, S4: the brute-force test above only walks node zooms and
    /// target zooms small enough to enumerate every descendant, so it never
    /// reaches the deep shifts (`delta` up to 31) the export cascade actually
    /// uses. This one pins the stronger, cheaper claim at those depths: the
    /// ranges of ALL `4^zn` nodes at a zoom *exactly tile* the target zoom's
    /// id space — sorted by start, they are gapless, non-overlapping, and
    /// their union is precisely `[base(target_z), base(target_z) + 4^target_z)`.
    /// A rotation bug (the premise the doc comment used to get wrong) would
    /// show up as a duplicate or a gap here, not as a wrong endpoint.
    #[test]
    fn node_id_ranges_partition_the_target_zoom_id_space() {
        for zn in 0u8..=6 {
            let side = 1u32 << zn;
            for target_z in [15u8, 25, 31] {
                let mut ranges: Vec<(u64, u64)> = Vec::with_capacity((side as usize).pow(2));
                for x in 0..side {
                    for y in 0..side {
                        let r = node_id_range(TileCoord::new(x, y, zn), target_z);
                        ranges.push((*r.start(), *r.end()));
                    }
                }
                ranges.sort_unstable();

                let span = 1u64 << (2 * u32::from(target_z - zn));
                let base = hilbert_zoom_base(target_z);
                let total = 1u64 << (2 * u32::from(target_z));
                let mut expected_start = base;
                for &(start, end) in &ranges {
                    assert_eq!(
                        start, expected_start,
                        "z{zn} -> z{target_z}: ranges are not gapless/disjoint at {start}"
                    );
                    assert_eq!(
                        end - start + 1,
                        span,
                        "z{zn} -> z{target_z}: every node owns exactly 4^delta ids"
                    );
                    expected_start = end + 1;
                }
                assert_eq!(
                    expected_start,
                    base + total,
                    "z{zn} -> z{target_z}: the union must be the whole zoom's id space"
                );
            }
        }
    }

    /// #506 review, S4: the direct statement of what the export cascade relies
    /// on — a tile's own id lies inside the range of EVERY one of its
    /// ancestors, at every ancestor zoom — sampled over 200k random z20 tiles
    /// (4.2M containment checks). The cascade prunes a subtree the moment a
    /// node's range misses the partition window, so a single ancestor whose
    /// range excludes one of its own descendants would silently drop that
    /// tile from the archive.
    #[test]
    fn node_id_range_contains_every_descendant_at_every_ancestor_zoom() {
        const TARGET_Z: u8 = 20;
        const SAMPLES: u32 = 200_000;
        let side = 1u32 << TARGET_Z;
        // Deterministic xorshift64* — no `rand` dependency, and a failure is
        // reproducible from the seed alone.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        for _ in 0..SAMPLES {
            let r = next();
            let x = (r as u32) % side;
            let y = ((r >> 32) as u32) % side;
            let id = crate::pmtiles_writer::tile_id(TARGET_Z, x, y);
            for za in 0..=TARGET_Z {
                let d = TARGET_Z - za;
                let ancestor = TileCoord::new(x >> d, y >> d, za);
                let range = node_id_range(ancestor, TARGET_Z);
                assert!(
                    range.contains(&id),
                    "z{TARGET_Z} tile ({x},{y}) id {id} is outside its z{za} ancestor \
                     ({},{})'s range {:?}",
                    ancestor.x,
                    ancestor.y,
                    range
                );
            }
        }
    }

    /// F2 (#506 review): `target_z` past [`crate::pmtiles_writer::MAX_TILE_ID_ZOOM`]
    /// must fail loudly in a debug build rather than silently shift by >= 64
    /// bits (masked to a near-zero shift in release -- see `node_id_range`'s
    /// doc).
    #[test]
    #[should_panic(expected = "exceeds the deepest zoom")]
    fn node_id_range_rejects_target_z_past_max_tile_id_zoom() {
        let _ = node_id_range(
            TileCoord::new(0, 0, 0),
            crate::pmtiles_writer::MAX_TILE_ID_ZOOM + 1,
        );
    }

    /// F2 (#506 review): `target_z < node.z` has no valid descendant range
    /// (node.z's Hilbert index and target_z's cumulative base would be
    /// different zooms' incompatible units) and must fail loudly rather than
    /// silently collapse `delta` to 0 via `saturating_sub`.
    #[test]
    #[should_panic(expected = "must be >= node.z")]
    fn node_id_range_rejects_target_z_below_node_z() {
        let _ = node_id_range(TileCoord::new(0, 0, 5), 3);
    }

    #[test]
    fn test_lng_lat_to_tile_exact_mercator_bound_maps_to_edge_rows() {
        // The Web Mercator latitude bound must map to the top row (y=0) and
        // its mirror to the bottom row (y = 2^z - 1) at every zoom level.
        //
        // Regression for #416: clamping to the shaved literal ±85.05 instead
        // of the exact bound moved these points several rows away from the
        // tile edge at z>=15 (e.g. row 38 instead of 0 at z20), silently
        // dropping the top/bottom Web Mercator band for any dataset whose
        // extent reaches the true Web Mercator limit.
        //
        // Three latitudes per hemisphere: the bound itself (exercises the
        // clamp's no-op path at its exact boundary), a value one ULP outside
        // it (exercises the clamp), and the pole (the projection's actual
        // divergence, far outside the clamp).
        const JUST_OUTSIDE: f64 = 85.051_128_779_806_6;
        // Compile-time guard: JUST_OUTSIDE only exercises the clamp while it
        // sits above MAX_LATITUDE, which also pins the constant to the "round
        // down" side of the true bound.
        const { assert!(JUST_OUTSIDE > MAX_LATITUDE) };

        for zoom in [0u8, 5, 10, 15, 18, 20, 22] {
            let max_valid = 2_u32.pow(zoom as u32) - 1;

            for lat in [MAX_LATITUDE, JUST_OUTSIDE, 90.0] {
                let north = lng_lat_to_tile(0.0, lat, zoom);
                assert_eq!(
                    north.y, 0,
                    "lat={lat} at zoom {zoom} should map to y=0, got {}",
                    north.y
                );
            }

            for lat in [-MAX_LATITUDE, -JUST_OUTSIDE, -90.0] {
                let south = lng_lat_to_tile(0.0, lat, zoom);
                assert_eq!(
                    south.y, max_valid,
                    "lat={lat} at zoom {zoom} should map to y={max_valid}, got {}",
                    south.y
                );
            }
        }
    }
}
