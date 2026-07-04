//! MVT (Mapbox Vector Tile) encoding module.
//!
//! This module implements the MVT specification for encoding geometries
//! into vector tiles. Key components:
//!
//! - **Zigzag encoding**: Efficiently encode signed integers as unsigned
//! - **Delta encoding**: Store coordinates as differences from previous position
//! - **Command encoding**: Pack geometry commands (MoveTo, LineTo, ClosePath)
//! - **Feature encoding**: Convert geo::Geometry to MVT Feature
//! - **Layer encoding**: Group features with deduplicated keys/values
//!
//! Reference: <https://github.com/mapbox/vector-tile-spec>

use crate::tile::TileBounds;
use crate::vector_tile::tile::{Feature, GeomType, Layer, Value};
use crate::vector_tile::Tile;
use geo::orient::{Direction, Orient};
use geo::{Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use std::collections::HashMap;

/// Default tile extent (4096 as per MVT spec)
pub const DEFAULT_EXTENT: u32 = 4096;

/// MVT command IDs
const CMD_MOVE_TO: u32 = 1;
const CMD_LINE_TO: u32 = 2;
const CMD_CLOSE_PATH: u32 = 7;

// ============================================================================
// Zigzag Encoding
// ============================================================================

/// Encode a signed integer using zigzag encoding.
///
/// Zigzag encoding maps signed integers to unsigned integers so that
/// small negative numbers have small encoded values:
/// - 0 → 0
/// - -1 → 1
/// - 1 → 2
/// - -2 → 3
/// - 2 → 4
/// - etc.
///
/// This is efficient for protobuf varint encoding since small values
/// use fewer bytes.
#[inline]
pub fn zigzag_encode(n: i32) -> u32 {
    ((n << 1) ^ (n >> 31)) as u32
}

/// Decode a zigzag-encoded unsigned integer back to signed.
#[inline]
pub fn zigzag_decode(n: u32) -> i32 {
    ((n >> 1) as i32) ^ -((n & 1) as i32)
}

// ============================================================================
// Command Encoding
// ============================================================================

/// Pack a command with a repeat count.
///
/// MVT commands are packed as: `(command_id | (count << 3))`
/// - command_id: 1=MoveTo, 2=LineTo, 7=ClosePath
/// - count: number of times to repeat the command
#[inline]
pub fn command_encode(command_id: u32, count: u32) -> u32 {
    (command_id & 0x7) | (count << 3)
}

/// Unpack a command into (command_id, count).
#[inline]
pub fn command_decode(command: u32) -> (u32, u32) {
    (command & 0x7, command >> 3)
}

// ============================================================================
// Winding Order Correction
// ============================================================================

/// Orient a polygon for MVT encoding.
///
/// MVT spec 4.3.3.3 defines ring roles by the sign of the surveyor's-formula
/// (shoelace) area computed on the stored tile coordinates:
/// - Exterior rings: POSITIVE area (appears clockwise with Y pointing down)
/// - Interior rings: NEGATIVE area (appears counter-clockwise with Y down)
///
/// Our coordinate transform flips Y (geographic latitude up → tile Y down),
/// and a Y-flip NEGATES the shoelace sign. So to end up positive in tile
/// coordinates, exterior rings must be NEGATIVE (clockwise) in geographic
/// coordinates — geo's `Direction::Reversed` convention:
/// - Exterior rings: clockwise in geo coords (positive area after Y-flip)
/// - Interior rings: counter-clockwise in geo coords (negative after Y-flip)
///
/// (An earlier version used `Direction::Default`, reasoning visually that
/// "geographic CCW appears CW after the Y-flip" — true on screen, but the
/// spec's definition is the algebraic sign on the stored coordinates, which
/// the flip negates. That emitted spec-inverted windings; fixed as part of
/// issue #112, whose decoder follows the spec sign.)
///
/// # Arguments
/// * `polygon` - The polygon to orient
///
/// # Returns
/// A new polygon with correctly oriented rings for MVT encoding
pub fn orient_polygon_for_mvt(polygon: &Polygon) -> Polygon {
    polygon.orient(Direction::Reversed)
}

/// Orient a multi-polygon for MVT encoding.
///
/// Applies `orient_polygon_for_mvt` to each constituent polygon.
///
/// # Arguments
/// * `multi` - The multi-polygon to orient
///
/// # Returns
/// A new multi-polygon with correctly oriented rings for MVT encoding
pub fn orient_multi_polygon_for_mvt(multi: &MultiPolygon) -> MultiPolygon {
    multi.orient(Direction::Reversed)
}

// ============================================================================
// Coordinate Transformation
// ============================================================================

/// Transform geographic coordinates (lng/lat) to tile-local coordinates.
///
/// Tile coordinates range from 0 to extent (typically 4096).
/// The tile bounds define the geographic extent being mapped.
///
/// # Arguments
/// * `lng` - Longitude in degrees
/// * `lat` - Latitude in degrees
/// * `bounds` - The geographic bounds of the tile
/// * `extent` - The tile extent (default 4096)
///
/// # Returns
/// (x, y) in tile-local coordinates, where (0,0) is top-left
pub fn geo_to_tile_coords(lng: f64, lat: f64, bounds: &TileBounds, extent: u32) -> (i32, i32) {
    let (x, y) = geo_to_tile_coords_unrounded(lng, lat, bounds, extent);
    (x.round() as i32, y.round() as i32)
}

/// Web Mercator Y fraction of a latitude: 0.0 at the top of the Mercator
/// world (+85.0511°), 0.5 at the equator, 1.0 at the bottom (−85.0511°).
///
/// Latitude is clamped to ±89.9° only to keep `tan` finite; callers passing
/// buffered coordinates slightly outside the Mercator range still get
/// monotonic (out-of-range) fractions rather than infinities.
#[inline]
pub(crate) fn mercator_y_fraction(lat: f64) -> f64 {
    let lat = lat.clamp(-89.9, 89.9);
    (1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0
}

/// f64 (unrounded) core of [`geo_to_tile_coords`], shared with the
/// simplification and feature-drop paths so filtering/simplification sees the
/// exact coordinates MVT encoding will produce.
///
/// Longitude is linear in Web Mercator X, so X interpolates linearly between
/// the tile's degree bounds. Latitude is NOT linear in Web Mercator Y: tile
/// bounds are Mercator-derived (see `TileCoord::bounds`), so Y must
/// interpolate in Mercator fraction space. Linear latitude interpolation
/// displaces features toward the poles — ~470/4096 units at z0 for 40.7°N —
/// shrinking below one unit only around z12.
#[inline]
pub(crate) fn geo_to_tile_coords_unrounded(
    lng: f64,
    lat: f64,
    bounds: &TileBounds,
    extent: u32,
) -> (f64, f64) {
    let extent_f = extent as f64;

    // X: linear in longitude.
    let x_ratio = (lng - bounds.lng_min) / (bounds.lng_max - bounds.lng_min);

    // Y: linear in Mercator fraction, top-down (tile Y increases downward).
    let merc_top = mercator_y_fraction(bounds.lat_max);
    let merc_bottom = mercator_y_fraction(bounds.lat_min);
    let y_ratio = (mercator_y_fraction(lat) - merc_top) / (merc_bottom - merc_top);

    (x_ratio * extent_f, y_ratio * extent_f)
}

// ============================================================================
// Geometry Encoding
// ============================================================================

/// Encode a Point geometry to MVT geometry commands.
pub fn encode_point(point: &Point, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    let (x, y) = geo_to_tile_coords(point.x(), point.y(), bounds, extent);

    vec![
        command_encode(CMD_MOVE_TO, 1),
        zigzag_encode(x),
        zigzag_encode(y),
    ]
}

/// Encode a MultiPoint geometry to MVT geometry commands.
pub fn encode_multi_point(points: &MultiPoint, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    if points.0.is_empty() {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(1 + points.0.len() * 2);
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    // All points use MoveTo with count = number of points
    geometry.push(command_encode(CMD_MOVE_TO, points.0.len() as u32));

    for point in &points.0 {
        let (x, y) = geo_to_tile_coords(point.x(), point.y(), bounds, extent);
        let dx = x - cursor_x;
        let dy = y - cursor_y;
        geometry.push(zigzag_encode(dx));
        geometry.push(zigzag_encode(dy));
        cursor_x = x;
        cursor_y = y;
    }

    geometry
}

/// Encode a LineString geometry to MVT geometry commands.
pub fn encode_linestring(line: &LineString, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    if line.0.len() < 2 {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(3 + (line.0.len() - 1) * 2);
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    // First point: MoveTo
    let first = &line.0[0];
    let (x, y) = geo_to_tile_coords(first.x, first.y, bounds, extent);
    let dx = x - cursor_x;
    let dy = y - cursor_y;
    geometry.push(command_encode(CMD_MOVE_TO, 1));
    geometry.push(zigzag_encode(dx));
    geometry.push(zigzag_encode(dy));
    cursor_x = x;
    cursor_y = y;

    // Remaining points: LineTo
    if line.0.len() > 1 {
        geometry.push(command_encode(CMD_LINE_TO, (line.0.len() - 1) as u32));
        for coord in line.0.iter().skip(1) {
            let (x, y) = geo_to_tile_coords(coord.x, coord.y, bounds, extent);
            let dx = x - cursor_x;
            let dy = y - cursor_y;
            geometry.push(zigzag_encode(dx));
            geometry.push(zigzag_encode(dy));
            cursor_x = x;
            cursor_y = y;
        }
    }

    geometry
}

/// Encode a MultiLineString geometry to MVT geometry commands.
pub fn encode_multi_linestring(
    lines: &MultiLineString,
    bounds: &TileBounds,
    extent: u32,
) -> Vec<u32> {
    let mut geometry = Vec::new();
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    for line in &lines.0 {
        if line.0.len() < 2 {
            continue;
        }

        // First point: MoveTo
        let first = &line.0[0];
        let (x, y) = geo_to_tile_coords(first.x, first.y, bounds, extent);
        let dx = x - cursor_x;
        let dy = y - cursor_y;
        geometry.push(command_encode(CMD_MOVE_TO, 1));
        geometry.push(zigzag_encode(dx));
        geometry.push(zigzag_encode(dy));
        cursor_x = x;
        cursor_y = y;

        // Remaining points: LineTo
        if line.0.len() > 1 {
            geometry.push(command_encode(CMD_LINE_TO, (line.0.len() - 1) as u32));
            for coord in line.0.iter().skip(1) {
                let (x, y) = geo_to_tile_coords(coord.x, coord.y, bounds, extent);
                let dx = x - cursor_x;
                let dy = y - cursor_y;
                geometry.push(zigzag_encode(dx));
                geometry.push(zigzag_encode(dy));
                cursor_x = x;
                cursor_y = y;
            }
        }
    }

    geometry
}

/// Encode a polygon ring (exterior or interior) to MVT geometry commands.
/// Returns the commands and updates the cursor position.
fn encode_ring(
    ring: &LineString,
    bounds: &TileBounds,
    extent: u32,
    cursor_x: &mut i32,
    cursor_y: &mut i32,
) -> Vec<u32> {
    // Rings must have at least 4 points (3 unique + closing point)
    if ring.0.len() < 4 {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(4 + (ring.0.len() - 2) * 2);

    // First point: MoveTo
    let first = &ring.0[0];
    let (x, y) = geo_to_tile_coords(first.x, first.y, bounds, extent);
    let dx = x - *cursor_x;
    let dy = y - *cursor_y;
    geometry.push(command_encode(CMD_MOVE_TO, 1));
    geometry.push(zigzag_encode(dx));
    geometry.push(zigzag_encode(dy));
    *cursor_x = x;
    *cursor_y = y;

    // Interior points: LineTo (skip last point since we'll use ClosePath)
    let line_to_count = ring.0.len() - 2; // Exclude first and last points
    if line_to_count > 0 {
        geometry.push(command_encode(CMD_LINE_TO, line_to_count as u32));
        for coord in ring.0.iter().skip(1).take(line_to_count) {
            let (x, y) = geo_to_tile_coords(coord.x, coord.y, bounds, extent);
            let dx = x - *cursor_x;
            let dy = y - *cursor_y;
            geometry.push(zigzag_encode(dx));
            geometry.push(zigzag_encode(dy));
            *cursor_x = x;
            *cursor_y = y;
        }
    }

    // ClosePath (implicitly returns to first point)
    geometry.push(command_encode(CMD_CLOSE_PATH, 1));

    geometry
}

/// Encode a Polygon geometry to MVT geometry commands.
///
/// This function automatically corrects polygon winding order to comply with
/// the MVT specification before encoding:
/// - Exterior rings: clockwise in tile coordinates
/// - Interior rings: counter-clockwise in tile coordinates
pub fn encode_polygon(polygon: &Polygon, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    // Apply winding order correction for MVT compliance
    let oriented = orient_polygon_for_mvt(polygon);

    let mut geometry = Vec::new();
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    // Exterior ring
    let exterior_cmds = encode_ring(
        oriented.exterior(),
        bounds,
        extent,
        &mut cursor_x,
        &mut cursor_y,
    );
    geometry.extend(exterior_cmds);

    // Interior rings (holes)
    for interior in oriented.interiors() {
        let interior_cmds = encode_ring(interior, bounds, extent, &mut cursor_x, &mut cursor_y);
        geometry.extend(interior_cmds);
    }

    geometry
}

/// Encode a MultiPolygon geometry to MVT geometry commands.
///
/// This function automatically corrects polygon winding order to comply with
/// the MVT specification before encoding:
/// - Exterior rings: clockwise in tile coordinates
/// - Interior rings: counter-clockwise in tile coordinates
pub fn encode_multi_polygon(polygons: &MultiPolygon, bounds: &TileBounds, extent: u32) -> Vec<u32> {
    // Apply winding order correction for MVT compliance
    let oriented = orient_multi_polygon_for_mvt(polygons);

    let mut geometry = Vec::new();
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    for polygon in &oriented.0 {
        // Exterior ring
        let exterior_cmds = encode_ring(
            polygon.exterior(),
            bounds,
            extent,
            &mut cursor_x,
            &mut cursor_y,
        );
        geometry.extend(exterior_cmds);

        // Interior rings (holes)
        for interior in polygon.interiors() {
            let interior_cmds = encode_ring(interior, bounds, extent, &mut cursor_x, &mut cursor_y);
            geometry.extend(interior_cmds);
        }
    }

    geometry
}

/// Encode any geo::Geometry to MVT geometry commands and return the geometry type.
pub fn encode_geometry(geom: &Geometry, bounds: &TileBounds, extent: u32) -> (Vec<u32>, GeomType) {
    match geom {
        Geometry::Point(p) => (encode_point(p, bounds, extent), GeomType::Point),
        Geometry::MultiPoint(mp) => (encode_multi_point(mp, bounds, extent), GeomType::Point),
        Geometry::LineString(ls) => (encode_linestring(ls, bounds, extent), GeomType::Linestring),
        Geometry::MultiLineString(mls) => (
            encode_multi_linestring(mls, bounds, extent),
            GeomType::Linestring,
        ),
        Geometry::Polygon(p) => (encode_polygon(p, bounds, extent), GeomType::Polygon),
        Geometry::MultiPolygon(mp) => (encode_multi_polygon(mp, bounds, extent), GeomType::Polygon),
        // For geometry collections, we'd need to handle each part separately
        // For now, return empty geometry with unknown type
        _ => (vec![], GeomType::Unknown),
    }
}

// ============================================================================
// Feature Encoding
// ============================================================================

/// A property value that can be encoded in MVT.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    String(String),
    Float(f32),
    Double(f64),
    Int(i64),
    UInt(u64),
    Bool(bool),
}

impl PropertyValue {
    /// Convert to MVT Value type.
    pub fn to_mvt_value(&self) -> Value {
        match self {
            PropertyValue::String(s) => Value {
                string_value: Some(s.clone()),
                ..Default::default()
            },
            PropertyValue::Float(f) => Value {
                float_value: Some(*f),
                ..Default::default()
            },
            PropertyValue::Double(d) => Value {
                double_value: Some(*d),
                ..Default::default()
            },
            PropertyValue::Int(i) => Value {
                int_value: Some(*i),
                ..Default::default()
            },
            PropertyValue::UInt(u) => Value {
                uint_value: Some(*u),
                ..Default::default()
            },
            PropertyValue::Bool(b) => Value {
                bool_value: Some(*b),
                ..Default::default()
            },
        }
    }
}

/// Builder for encoding features into an MVT layer.
pub struct LayerBuilder {
    name: String,
    extent: u32,
    features: Vec<Feature>,
    keys: Vec<String>,
    key_index: HashMap<String, u32>,
    values: Vec<Value>,
    value_index: HashMap<String, u32>, // Serialize value for deduplication lookup
}

impl LayerBuilder {
    /// Create a new layer builder with the given name.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            extent: DEFAULT_EXTENT,
            features: Vec::new(),
            keys: Vec::new(),
            key_index: HashMap::new(),
            values: Vec::new(),
            value_index: HashMap::new(),
        }
    }

    /// Set the layer extent.
    pub fn with_extent(mut self, extent: u32) -> Self {
        self.extent = extent;
        self
    }

    /// Get or insert a key, returning its index.
    fn get_or_insert_key(&mut self, key: &str) -> u32 {
        if let Some(&idx) = self.key_index.get(key) {
            idx
        } else {
            let idx = self.keys.len() as u32;
            self.keys.push(key.to_string());
            self.key_index.insert(key.to_string(), idx);
            idx
        }
    }

    /// Get or insert a value, returning its index.
    fn get_or_insert_value(&mut self, value: &PropertyValue) -> u32 {
        // Create a string key for deduplication
        let value_key = format!("{:?}", value);

        if let Some(&idx) = self.value_index.get(&value_key) {
            idx
        } else {
            let idx = self.values.len() as u32;
            self.values.push(value.to_mvt_value());
            self.value_index.insert(value_key, idx);
            idx
        }
    }

    /// Add a feature to the layer.
    ///
    /// # Arguments
    /// * `id` - Optional feature ID
    /// * `geometry` - The geometry to encode
    /// * `properties` - Feature properties as key-value pairs
    /// * `bounds` - The tile bounds for coordinate transformation
    pub fn add_feature(
        &mut self,
        id: Option<u64>,
        geometry: &Geometry,
        properties: &[(String, PropertyValue)],
        bounds: &TileBounds,
    ) {
        let (geom_commands, geom_type) = encode_geometry(geometry, bounds, self.extent);

        // Skip empty geometries
        if geom_commands.is_empty() && geom_type == GeomType::Unknown {
            return;
        }

        // Encode tags as [key_idx, value_idx, key_idx, value_idx, ...]
        let mut tags = Vec::with_capacity(properties.len() * 2);
        for (key, value) in properties {
            let key_idx = self.get_or_insert_key(key);
            let value_idx = self.get_or_insert_value(value);
            tags.push(key_idx);
            tags.push(value_idx);
        }

        let feature = Feature {
            id,
            tags,
            r#type: Some(geom_type as i32),
            geometry: geom_commands,
        };

        self.features.push(feature);
    }

    /// Build the MVT Layer.
    pub fn build(self) -> Layer {
        Layer {
            version: 2,
            name: self.name,
            features: self.features,
            keys: self.keys,
            values: self.values,
            extent: Some(self.extent),
        }
    }
}

/// Builder for encoding multiple layers into an MVT tile.
pub struct TileBuilder {
    layers: Vec<Layer>,
}

impl TileBuilder {
    /// Create a new tile builder.
    pub fn new() -> Self {
        Self { layers: Vec::new() }
    }

    /// Add a layer to the tile.
    pub fn add_layer(&mut self, layer: Layer) {
        self.layers.push(layer);
    }

    /// Build the MVT Tile.
    pub fn build(self) -> Tile {
        Tile {
            layers: self.layers,
        }
    }
}

impl Default for TileBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::TileCoord;
    use geo::{line_string, point, polygon};

    // ------------------------------------------------------------------------
    // Zigzag Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_zigzag_encode_zero() {
        assert_eq!(zigzag_encode(0), 0);
    }

    #[test]
    fn test_zigzag_encode_negative_one() {
        assert_eq!(zigzag_encode(-1), 1);
    }

    #[test]
    fn test_zigzag_encode_positive_one() {
        assert_eq!(zigzag_encode(1), 2);
    }

    #[test]
    fn test_zigzag_encode_negative_two() {
        assert_eq!(zigzag_encode(-2), 3);
    }

    #[test]
    fn test_zigzag_encode_positive_two() {
        assert_eq!(zigzag_encode(2), 4);
    }

    #[test]
    fn test_zigzag_encode_large_positive() {
        // 100 → 200
        assert_eq!(zigzag_encode(100), 200);
    }

    #[test]
    fn test_zigzag_encode_large_negative() {
        // -100 → 199
        assert_eq!(zigzag_encode(-100), 199);
    }

    #[test]
    fn test_zigzag_roundtrip() {
        for n in -1000..=1000 {
            let encoded = zigzag_encode(n);
            let decoded = zigzag_decode(encoded);
            assert_eq!(decoded, n, "Roundtrip failed for {}", n);
        }
    }

    #[test]
    fn test_zigzag_decode() {
        assert_eq!(zigzag_decode(0), 0);
        assert_eq!(zigzag_decode(1), -1);
        assert_eq!(zigzag_decode(2), 1);
        assert_eq!(zigzag_decode(3), -2);
        assert_eq!(zigzag_decode(4), 2);
    }

    // ------------------------------------------------------------------------
    // Command Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_command_encode_moveto_1() {
        // MoveTo with count=1: (1 | (1 << 3)) = 9
        assert_eq!(command_encode(CMD_MOVE_TO, 1), 9);
    }

    #[test]
    fn test_command_encode_lineto_3() {
        // LineTo with count=3: (2 | (3 << 3)) = 26
        assert_eq!(command_encode(CMD_LINE_TO, 3), 26);
    }

    #[test]
    fn test_command_encode_closepath() {
        // ClosePath with count=1: (7 | (1 << 3)) = 15
        assert_eq!(command_encode(CMD_CLOSE_PATH, 1), 15);
    }

    #[test]
    fn test_command_decode() {
        let cmd = command_encode(CMD_LINE_TO, 5);
        let (id, count) = command_decode(cmd);
        assert_eq!(id, CMD_LINE_TO);
        assert_eq!(count, 5);
    }

    #[test]
    fn test_command_roundtrip() {
        for cmd_id in [CMD_MOVE_TO, CMD_LINE_TO, CMD_CLOSE_PATH] {
            for count in 1..=100 {
                let encoded = command_encode(cmd_id, count);
                let (decoded_id, decoded_count) = command_decode(encoded);
                assert_eq!(decoded_id, cmd_id);
                assert_eq!(decoded_count, count);
            }
        }
    }

    // ------------------------------------------------------------------------
    // Coordinate Transformation Tests
    // ------------------------------------------------------------------------

    fn test_bounds() -> TileBounds {
        TileBounds {
            lng_min: 0.0,
            lat_min: 0.0,
            lng_max: 1.0,
            lat_max: 1.0,
        }
    }

    #[test]
    fn test_geo_to_tile_coords_center() {
        let bounds = test_bounds();
        let (x, y) = geo_to_tile_coords(0.5, 0.5, &bounds, 4096);
        // Center should be (2048, 2048)
        assert_eq!(x, 2048);
        assert_eq!(y, 2048);
    }

    #[test]
    fn test_geo_to_tile_coords_origin() {
        let bounds = test_bounds();
        // Bottom-left corner (lng_min, lat_min) → (0, extent) since Y is flipped
        let (x, y) = geo_to_tile_coords(0.0, 0.0, &bounds, 4096);
        assert_eq!(x, 0);
        assert_eq!(y, 4096);
    }

    #[test]
    fn test_geo_to_tile_coords_top_right() {
        let bounds = test_bounds();
        // Top-right corner (lng_max, lat_max) → (extent, 0)
        let (x, y) = geo_to_tile_coords(1.0, 1.0, &bounds, 4096);
        assert_eq!(x, 4096);
        assert_eq!(y, 0);
    }

    #[test]
    fn test_geo_to_tile_coords_top_left() {
        let bounds = test_bounds();
        // Top-left corner (lng_min, lat_max) → (0, 0)
        let (x, y) = geo_to_tile_coords(0.0, 1.0, &bounds, 4096);
        assert_eq!(x, 0);
        assert_eq!(y, 0);
    }

    /// Web-Mercator-correct expected tile-local Y for a latitude at a zoom,
    /// computed independently of production code:
    /// merc fraction y = (1 - ln(tan(φ) + 1/cos(φ)) / π) / 2,
    /// tile-local = (y * 2^z - tile_y) * extent.
    fn expected_mercator_tile_y(lat: f64, zoom: u8, tile_y: u32, extent: u32) -> i32 {
        let phi = lat.to_radians();
        let merc = (1.0 - (phi.tan() + 1.0 / phi.cos()).ln() / std::f64::consts::PI) / 2.0;
        ((merc * (1u32 << zoom) as f64 - tile_y as f64) * extent as f64).round() as i32
    }

    #[test]
    fn test_geo_to_tile_coords_mercator_y_z0() {
        // NYC (40.7128°N, -74.0060°W) in the single z0 tile. Latitude must be
        // placed with Web Mercator Y, not linear interpolation between the
        // tile's degree bounds. Linear interpolation puts this ~472 units too
        // far north (y≈1068 instead of 1540), i.e. renders NYC at ~65°N.
        let (lng, lat) = (-74.0060, 40.7128);
        let tile = TileCoord::new(0, 0, 0);
        let bounds = tile.bounds();
        let extent = 4096;

        let expected_y = expected_mercator_tile_y(lat, 0, 0, extent);
        let expected_x = (((lng + 180.0) / 360.0) * extent as f64).round() as i32;

        let (x, y) = geo_to_tile_coords(lng, lat, &bounds, extent);
        assert!(
            (x - expected_x).abs() <= 1,
            "z0 X: got {x}, expected {expected_x}"
        );
        assert!(
            (y - expected_y).abs() <= 1,
            "z0 Y must be Web Mercator: got {y}, expected {expected_y}"
        );
    }

    #[test]
    fn test_geo_to_tile_coords_mercator_y_z2() {
        // Same point in its z2 tile (x=1, y=1). Linear interpolation still
        // misplaces latitude by tens of units at z2.
        let (lng, lat) = (-74.0060, 40.7128);
        let tile = TileCoord::new(1, 1, 2);
        let bounds = tile.bounds();
        let extent = 4096;

        let expected_y = expected_mercator_tile_y(lat, 2, 1, extent);
        let (_, y) = geo_to_tile_coords(lng, lat, &bounds, extent);
        assert!(
            (y - expected_y).abs() <= 1,
            "z2 Y must be Web Mercator: got {y}, expected {expected_y}"
        );
    }

    #[test]
    fn test_geo_to_tile_coords_mercator_y_z12_regression() {
        // Fine-zoom regression pin: at z12 a tile spans so few degrees that
        // mercator and linear agree to sub-unit precision — fine zooms were
        // visually correct before the mercator fix and must not shift.
        let (lng, lat) = (-74.0060_f64, 40.7128_f64);
        let zoom = 12u8;
        let n = 1u32 << zoom;
        let tx = (((lng + 180.0) / 360.0) * n as f64).floor() as u32;
        let phi = lat.to_radians();
        let merc = (1.0 - (phi.tan() + 1.0 / phi.cos()).ln() / std::f64::consts::PI) / 2.0;
        let ty = (merc * n as f64).floor() as u32;
        let tile = TileCoord::new(tx, ty, zoom);
        let bounds = tile.bounds();
        let extent = 4096;

        let expected_y = expected_mercator_tile_y(lat, zoom, ty, extent);
        // The pre-fix linear-interpolation value, pinned so the fix provably
        // does not move fine-zoom output by more than 1 unit.
        let linear_y = (((bounds.lat_max - lat) / (bounds.lat_max - bounds.lat_min))
            * extent as f64)
            .round() as i32;
        assert!(
            (expected_y - linear_y).abs() <= 1,
            "test premise: mercator and linear must agree at z12 \
             (mercator {expected_y} vs linear {linear_y})"
        );

        let (_, y) = geo_to_tile_coords(lng, lat, &bounds, extent);
        assert!(
            (y - expected_y).abs() <= 1,
            "z12 Y: got {y}, expected {expected_y} (pre-fix {linear_y})"
        );
    }

    // ------------------------------------------------------------------------
    // Point Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_point_at_center() {
        let bounds = test_bounds();
        let point = point!(x: 0.5, y: 0.5);
        let commands = encode_point(&point, &bounds, 4096);

        // Should be: [MoveTo(1), zigzag(2048), zigzag(2048)]
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1)); // 9
        assert_eq!(commands[1], zigzag_encode(2048)); // x
        assert_eq!(commands[2], zigzag_encode(2048)); // y
    }

    #[test]
    fn test_encode_point_at_origin() {
        let bounds = test_bounds();
        let point = point!(x: 0.0, y: 0.0); // Bottom-left
        let commands = encode_point(&point, &bounds, 4096);

        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1));
        assert_eq!(commands[1], zigzag_encode(0)); // x = 0
        assert_eq!(commands[2], zigzag_encode(4096)); // y = 4096 (flipped)
    }

    // ------------------------------------------------------------------------
    // LineString Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_linestring_simple() {
        let bounds = test_bounds();
        let line = line_string![
            (x: 0.0, y: 0.0),
            (x: 0.5, y: 0.5),
            (x: 1.0, y: 1.0),
        ];
        let commands = encode_linestring(&line, &bounds, 4096);

        // Should be: MoveTo(1), x, y, LineTo(2), dx1, dy1, dx2, dy2
        // That's 1 + 2 + 1 + 4 = 8 elements
        assert_eq!(commands.len(), 8);

        // MoveTo command
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1));

        // First point (0, 4096) - bottom left in tile coords
        assert_eq!(commands[1], zigzag_encode(0)); // x
        assert_eq!(commands[2], zigzag_encode(4096)); // y (flipped from lat)

        // LineTo command with count=2
        assert_eq!(commands[3], command_encode(CMD_LINE_TO, 2));

        // Delta to (2048, 2048) from (0, 4096) = (2048, -2048)
        assert_eq!(commands[4], zigzag_encode(2048));
        assert_eq!(commands[5], zigzag_encode(-2048));

        // Delta to (4096, 0) from (2048, 2048) = (2048, -2048)
        assert_eq!(commands[6], zigzag_encode(2048));
        assert_eq!(commands[7], zigzag_encode(-2048));
    }

    #[test]
    fn test_encode_linestring_too_short() {
        let bounds = test_bounds();
        let line = line_string![(x: 0.0, y: 0.0)]; // Only one point
        let commands = encode_linestring(&line, &bounds, 4096);

        // Should return empty - linestrings need at least 2 points
        assert!(commands.is_empty());
    }

    // ------------------------------------------------------------------------
    // Polygon Encoding Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_polygon_simple() {
        let bounds = test_bounds();
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0), // Closing point
        ];
        let commands = encode_polygon(&poly, &bounds, 4096);

        // Should have: MoveTo(1), x, y, LineTo(3), dx1, dy1, dx2, dy2, dx3, dy3, ClosePath(1)
        // MoveTo + 2 coords + LineTo + 6 coords + ClosePath = 10 elements
        assert!(!commands.is_empty());

        // First command should be MoveTo
        assert_eq!(command_decode(commands[0]).0, CMD_MOVE_TO);

        // Last command should be ClosePath
        let last_cmd = *commands.last().unwrap();
        assert_eq!(command_decode(last_cmd).0, CMD_CLOSE_PATH);
    }

    // ------------------------------------------------------------------------
    // Layer Builder Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_layer_builder_basic() {
        let bounds = test_bounds();
        let mut builder = LayerBuilder::new("test_layer");

        let point = Geometry::Point(point!(x: 0.5, y: 0.5));
        let properties = vec![
            (
                "name".to_string(),
                PropertyValue::String("test".to_string()),
            ),
            ("value".to_string(), PropertyValue::Int(42)),
        ];

        builder.add_feature(Some(1), &point, &properties, &bounds);

        let layer = builder.build();

        assert_eq!(layer.name, "test_layer");
        assert_eq!(layer.version, 2);
        assert_eq!(layer.features.len(), 1);
        assert_eq!(layer.keys.len(), 2);
        assert_eq!(layer.values.len(), 2);
        assert_eq!(layer.extent, Some(4096));
    }

    #[test]
    fn test_layer_builder_key_deduplication() {
        let bounds = test_bounds();
        let mut builder = LayerBuilder::new("test_layer");

        let point1 = Geometry::Point(point!(x: 0.25, y: 0.25));
        let point2 = Geometry::Point(point!(x: 0.75, y: 0.75));

        // Both features have "name" key - should be deduplicated
        let props1 = vec![("name".to_string(), PropertyValue::String("a".to_string()))];
        let props2 = vec![("name".to_string(), PropertyValue::String("b".to_string()))];

        builder.add_feature(Some(1), &point1, &props1, &bounds);
        builder.add_feature(Some(2), &point2, &props2, &bounds);

        let layer = builder.build();

        assert_eq!(layer.features.len(), 2);
        assert_eq!(layer.keys.len(), 1); // Only one unique key "name"
        assert_eq!(layer.values.len(), 2); // Two different values "a" and "b"
    }

    #[test]
    fn test_layer_builder_value_deduplication() {
        let bounds = test_bounds();
        let mut builder = LayerBuilder::new("test_layer");

        let point1 = Geometry::Point(point!(x: 0.25, y: 0.25));
        let point2 = Geometry::Point(point!(x: 0.75, y: 0.75));

        // Both features have same value - should be deduplicated
        let props1 = vec![(
            "type".to_string(),
            PropertyValue::String("building".to_string()),
        )];
        let props2 = vec![(
            "type".to_string(),
            PropertyValue::String("building".to_string()),
        )];

        builder.add_feature(Some(1), &point1, &props1, &bounds);
        builder.add_feature(Some(2), &point2, &props2, &bounds);

        let layer = builder.build();

        assert_eq!(layer.features.len(), 2);
        assert_eq!(layer.keys.len(), 1); // One key "type"
        assert_eq!(layer.values.len(), 1); // One value "building" (deduplicated)
    }

    // ------------------------------------------------------------------------
    // Tile Builder Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_tile_builder() {
        let bounds = test_bounds();

        let mut layer1 = LayerBuilder::new("points");
        layer1.add_feature(
            Some(1),
            &Geometry::Point(point!(x: 0.5, y: 0.5)),
            &[],
            &bounds,
        );

        let mut layer2 = LayerBuilder::new("lines");
        let line = Geometry::LineString(line_string![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 1.0),
        ]);
        layer2.add_feature(Some(2), &line, &[], &bounds);

        let mut tile_builder = TileBuilder::new();
        tile_builder.add_layer(layer1.build());
        tile_builder.add_layer(layer2.build());

        let tile = tile_builder.build();

        assert_eq!(tile.layers.len(), 2);
        assert_eq!(tile.layers[0].name, "points");
        assert_eq!(tile.layers[1].name, "lines");
    }

    // ------------------------------------------------------------------------
    // GeomType Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_encode_geometry_returns_correct_type() {
        let bounds = test_bounds();

        let (_, geom_type) =
            encode_geometry(&Geometry::Point(point!(x: 0.5, y: 0.5)), &bounds, 4096);
        assert_eq!(geom_type, GeomType::Point);

        let line = Geometry::LineString(line_string![(x: 0.0, y: 0.0), (x: 1.0, y: 1.0)]);
        let (_, geom_type) = encode_geometry(&line, &bounds, 4096);
        assert_eq!(geom_type, GeomType::Linestring);

        let poly = Geometry::Polygon(polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ]);
        let (_, geom_type) = encode_geometry(&poly, &bounds, 4096);
        assert_eq!(geom_type, GeomType::Polygon);
    }

    // ------------------------------------------------------------------------
    // Winding Order Tests
    // ------------------------------------------------------------------------

    #[test]
    fn test_polygon_correct_winding_unchanged() {
        // A polygon with correct MVT winding (CW exterior in geographic
        // coords: the Y-flip negates the shoelace sign, yielding the
        // POSITIVE tile-space area the spec requires for exterior rings)
        // should pass through unchanged.

        // CW polygon in geographic coords (correct for MVT after Y-flip)
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 0.0, y: 1.0),
            (x: 1.0, y: 1.0),
            (x: 1.0, y: 0.0),
            (x: 0.0, y: 0.0),
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Should be unchanged since it's already correctly oriented
        assert_eq!(poly.exterior().0, oriented.exterior().0);
    }

    #[test]
    fn test_polygon_incorrect_winding_gets_corrected() {
        // A polygon with incorrect winding (CCW exterior in geographic
        // coords, which would flip to NEGATIVE tile-space area) should be
        // corrected to CW exterior.

        // CCW polygon in geographic coords (incorrect - needs correction)
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Should now be CW (reversed from input)
        // The first and last points stay the same, but the middle points should be reversed
        assert_ne!(poly.exterior().0[1], oriented.exterior().0[1]);
    }

    #[test]
    fn test_encoded_exterior_ring_has_positive_tile_area() {
        // MVT spec 4.3.3.3: exterior rings must have POSITIVE area by the
        // surveyor's formula on tile coordinates; interior rings NEGATIVE.
        // This pins the winding fix (issue #112) at the command-stream level.
        let bounds = test_bounds();
        let poly = polygon![
            exterior: [
                (x: 0.1, y: 0.1),
                (x: 0.9, y: 0.1),
                (x: 0.9, y: 0.9),
                (x: 0.1, y: 0.9),
                (x: 0.1, y: 0.1),
            ],
            interiors: [
                [
                    (x: 0.4, y: 0.4),
                    (x: 0.6, y: 0.4),
                    (x: 0.6, y: 0.6),
                    (x: 0.4, y: 0.6),
                    (x: 0.4, y: 0.4),
                ],
            ],
        ];
        let commands = encode_polygon(&poly, &bounds, 4096);

        // Decode the command stream into rings of absolute tile coords.
        let mut rings: Vec<Vec<(i64, i64)>> = Vec::new();
        let mut cur: Vec<(i64, i64)> = Vec::new();
        let (mut cx, mut cy) = (0i64, 0i64);
        let mut i = 0;
        while i < commands.len() {
            let (cmd, count) = command_decode(commands[i]);
            i += 1;
            match cmd {
                CMD_MOVE_TO | CMD_LINE_TO => {
                    for _ in 0..count {
                        cx += i64::from(zigzag_decode(commands[i]));
                        cy += i64::from(zigzag_decode(commands[i + 1]));
                        i += 2;
                        cur.push((cx, cy));
                    }
                }
                CMD_CLOSE_PATH => rings.push(std::mem::take(&mut cur)),
                _ => panic!("unexpected command"),
            }
        }
        assert_eq!(rings.len(), 2, "exterior + hole");

        let area2 = |ring: &[(i64, i64)]| -> i64 {
            let n = ring.len();
            (0..n)
                .map(|j| {
                    let (x0, y0) = ring[j];
                    let (x1, y1) = ring[(j + 1) % n];
                    x0 * y1 - x1 * y0
                })
                .sum()
        };
        assert!(
            area2(&rings[0]) > 0,
            "exterior ring must have positive tile-space area, got {}",
            area2(&rings[0])
        );
        assert!(
            area2(&rings[1]) < 0,
            "interior ring must have negative tile-space area, got {}",
            area2(&rings[1])
        );
    }

    #[test]
    fn test_polygon_with_hole_correct_winding() {
        // A polygon with a hole should have:
        // - CW exterior in geographic coords (positive tile-space area)
        // - CCW interior in geographic coords (negative tile-space area)

        // Exterior: CW in geo coords (correct)
        // Interior: CCW in geo coords (correct for a hole)
        let poly = polygon![
            exterior: [
                (x: 0.0, y: 0.0),
                (x: 0.0, y: 10.0),
                (x: 10.0, y: 10.0),
                (x: 10.0, y: 0.0),
                (x: 0.0, y: 0.0),
            ],
            interiors: [
                [
                    (x: 2.0, y: 2.0),
                    (x: 8.0, y: 2.0),
                    (x: 8.0, y: 8.0),
                    (x: 2.0, y: 8.0),
                    (x: 2.0, y: 2.0),
                ],
            ],
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // After orientation, exterior stays CW and interior stays CCW
        // (in geographic coordinates: positive/negative tile-space area).
        assert_eq!(oriented.interiors().len(), 1);
        assert_eq!(poly.exterior().0, oriented.exterior().0);
        assert_eq!(poly.interiors()[0].0, oriented.interiors()[0].0);
    }

    #[test]
    fn test_polygon_with_hole_incorrect_winding_gets_corrected() {
        // A polygon where both exterior and interior have wrong winding

        // Exterior: CCW in geo coords (wrong)
        // Interior: CW in geo coords (wrong for a hole)
        let poly = polygon![
            exterior: [
                (x: 0.0, y: 0.0),
                (x: 10.0, y: 0.0),
                (x: 10.0, y: 10.0),
                (x: 0.0, y: 10.0),
                (x: 0.0, y: 0.0),
            ],
            interiors: [
                [
                    (x: 2.0, y: 2.0),
                    (x: 2.0, y: 8.0),
                    (x: 8.0, y: 8.0),
                    (x: 8.0, y: 2.0),
                    (x: 2.0, y: 2.0),
                ],
            ],
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Both should be corrected
        assert_ne!(poly.exterior().0[1], oriented.exterior().0[1]);
        assert_ne!(poly.interiors()[0].0[1], oriented.interiors()[0].0[1]);
    }

    #[test]
    fn test_multipolygon_winding_correction() {
        // MultiPolygon should have all constituent polygons corrected

        // First polygon: wrong winding
        // Second polygon: correct winding
        let multi = geo::MultiPolygon::new(vec![
            polygon![
                (x: 0.0, y: 0.0),
                (x: 0.0, y: 1.0),
                (x: 1.0, y: 1.0),
                (x: 1.0, y: 0.0),
                (x: 0.0, y: 0.0),
            ],
            polygon![
                (x: 2.0, y: 0.0),
                (x: 3.0, y: 0.0),
                (x: 3.0, y: 1.0),
                (x: 2.0, y: 1.0),
                (x: 2.0, y: 0.0),
            ],
        ]);

        let oriented = orient_multi_polygon_for_mvt(&multi);

        assert_eq!(oriented.0.len(), 2);
        // First polygon should be corrected (was CW, now CCW)
        // Second polygon should remain unchanged (was already CCW)
    }

    #[test]
    fn test_encode_polygon_applies_winding_correction() {
        // The main encode_polygon function should apply winding correction
        let bounds = test_bounds();

        // CW polygon (wrong winding in geographic coords)
        let poly_cw = polygon![
            (x: 0.0, y: 0.0),
            (x: 0.0, y: 1.0),
            (x: 1.0, y: 1.0),
            (x: 1.0, y: 0.0),
            (x: 0.0, y: 0.0),
        ];

        // CCW polygon (correct winding in geographic coords)
        let poly_ccw = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ];

        // Both should produce equivalent MVT output (same geometry, just different input winding)
        let commands_cw = encode_polygon(&poly_cw, &bounds, 4096);
        let commands_ccw = encode_polygon(&poly_ccw, &bounds, 4096);

        // The encoded geometry should be the same for both inputs
        // because winding correction normalizes them
        assert_eq!(commands_cw, commands_ccw);
    }

    /// Test to measure MVT encoding overhead for MultiLineString vs single LineString.
    ///
    /// Hypothesis: A MultiLineString with many short linestrings is MUCH larger than
    /// a single LineString with the same total points due to MoveTo command overhead.
    ///
    /// Each linestring in a MultiLineString requires:
    /// - MoveTo(1) command: 1 u32
    /// - First point coordinates: 2 u32 (zigzag encoded dx, dy)
    /// - LineTo(n-1) command: 1 u32
    /// - Remaining points: 2*(n-1) u32
    ///
    /// For a 2-point linestring, that's: 1 + 2 + 1 + 2 = 6 u32 per linestring
    /// For 100 2-point linestrings: 600 u32
    ///
    /// A single 200-point linestring:
    /// - MoveTo(1) command: 1 u32
    /// - First point: 2 u32
    /// - LineTo(199): 1 u32
    /// - Remaining 199 points: 398 u32
    ///
    /// Total: 402 u32
    ///
    /// Expected overhead ratio: ~1.5x for geometry commands alone,
    /// but protobuf varint encoding may amplify this.
    #[test]
    fn test_mvt_encoding_overhead_multilinestring_vs_linestring() {
        use geo::coord;
        use prost::Message;

        let bounds = TileBounds {
            lng_min: -180.0,
            lat_min: -85.0,
            lng_max: 180.0,
            lat_max: 85.0,
        };
        let extent = DEFAULT_EXTENT;

        // Create 100 separate 2-point linestrings (200 total points)
        let mut lines: Vec<LineString> = Vec::with_capacity(100);
        for i in 0..100 {
            // Each linestring spans a small portion of the tile
            let x1 = -180.0 + (i as f64 * 3.6); // spread across longitude
            let x2 = x1 + 1.0;
            let y = (i as f64) * 0.8 - 40.0; // spread across latitude
            let line = LineString::new(vec![coord! { x: x1, y: y }, coord! { x: x2, y: y }]);
            lines.push(line);
        }
        let multi_linestring = MultiLineString::new(lines);

        // Create a single linestring with 200 points
        let mut single_coords: Vec<geo::Coord<f64>> = Vec::with_capacity(200);
        for i in 0..200 {
            let x = -180.0 + (i as f64 * 1.8);
            let y = (i as f64) * 0.4 - 40.0;
            single_coords.push(coord! { x: x, y: y });
        }
        let single_linestring = LineString::new(single_coords);

        // Encode MultiLineString
        let multi_cmds = encode_multi_linestring(&multi_linestring, &bounds, extent);

        // Encode single LineString
        let single_cmds = encode_linestring(&single_linestring, &bounds, extent);

        println!("\n=== MVT Encoding Overhead Test ===");
        println!("MultiLineString: 100 linestrings x 2 points each = 200 total points");
        println!("Single LineString: 200 points");
        println!();
        println!("Geometry command counts:");
        println!("  MultiLineString: {} u32 values", multi_cmds.len());
        println!("  Single LineString: {} u32 values", single_cmds.len());
        println!(
            "  Command overhead ratio: {:.2}x",
            multi_cmds.len() as f64 / single_cmds.len() as f64
        );

        // Now encode to full MVT tiles and compare byte sizes
        let mut multi_layer = LayerBuilder::new("test");
        multi_layer.add_feature(
            Some(1),
            &Geometry::MultiLineString(multi_linestring.clone()),
            &[],
            &bounds,
        );
        let multi_tile = TileBuilder::new();
        let mut multi_builder = multi_tile;
        multi_builder.add_layer(multi_layer.build());
        let multi_tile_proto = multi_builder.build();
        let multi_bytes = multi_tile_proto.encode_to_vec();

        let mut single_layer = LayerBuilder::new("test");
        single_layer.add_feature(
            Some(1),
            &Geometry::LineString(single_linestring.clone()),
            &[],
            &bounds,
        );
        let single_tile = TileBuilder::new();
        let mut single_builder = single_tile;
        single_builder.add_layer(single_layer.build());
        let single_tile_proto = single_builder.build();
        let single_bytes = single_tile_proto.encode_to_vec();

        println!();
        println!("MVT protobuf byte sizes:");
        println!("  MultiLineString tile: {} bytes", multi_bytes.len());
        println!("  Single LineString tile: {} bytes", single_bytes.len());
        println!(
            "  Byte overhead ratio: {:.2}x",
            multi_bytes.len() as f64 / single_bytes.len() as f64
        );
        println!();
        println!(
            "Overhead per linestring: {} extra bytes",
            (multi_bytes.len() - single_bytes.len()) / 100
        );

        // Verify the hypothesis: MultiLineString should be significantly larger
        assert!(
            multi_cmds.len() > single_cmds.len(),
            "MultiLineString should have more command values than single LineString"
        );

        // The ratio should be around 1.5x for 2-point linestrings
        let cmd_ratio = multi_cmds.len() as f64 / single_cmds.len() as f64;
        assert!(
            cmd_ratio > 1.2,
            "Command overhead ratio should be >1.2x, got {:.2}x",
            cmd_ratio
        );
    }
}
