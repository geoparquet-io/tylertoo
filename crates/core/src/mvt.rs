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

use crate::tile::{TileBounds, TileCoord};
use crate::vector_tile::tile::{Feature, GeomType, Layer, Value};
use crate::vector_tile::Tile;
use crate::world_coord::WorldCoord;
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
/// MVT specification requires:
/// - Exterior rings: clockwise in screen/tile coordinates (positive area)
/// - Interior rings: counter-clockwise in screen/tile coordinates (negative area)
///
/// Since tile coordinates have Y increasing downward (opposite to geographic coords),
/// and our coordinate transform flips Y, we need polygons in geographic coordinates
/// to have:
/// - Exterior rings: counter-clockwise (becomes CW after Y-flip)
/// - Interior rings: clockwise (becomes CCW after Y-flip)
///
/// This matches geo's `Direction::Default` convention.
///
/// # Arguments
/// * `polygon` - The polygon to orient
///
/// # Returns
/// A new polygon with correctly oriented rings for MVT encoding
pub fn orient_polygon_for_mvt(polygon: &Polygon) -> Polygon {
    polygon.orient(Direction::Default)
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
    multi.orient(Direction::Default)
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
    let extent_f = extent as f64;

    // Normalize to 0-1 within tile bounds
    let x_ratio = (lng - bounds.lng_min) / (bounds.lng_max - bounds.lng_min);
    let y_ratio = (lat - bounds.lat_min) / (bounds.lat_max - bounds.lat_min);

    // Scale to extent and flip Y (tile coords have Y increasing downward)
    let x = (x_ratio * extent_f).round() as i32;
    let y = ((1.0 - y_ratio) * extent_f).round() as i32;

    (x, y)
}

// ============================================================================
// WorldCoord → Tile-Local Coordinate Transformation
// ============================================================================

/// Transform a WorldCoord to tile-local MVT coordinates.
///
/// This is the integer-coordinate equivalent of `geo_to_tile_coords`.
/// It uses `WorldCoord::to_tile_local` for the projection, which avoids
/// floating-point imprecision in the coordinate transformation pipeline.
///
/// # Arguments
/// * `coord` - World coordinate to transform
/// * `tile` - Target tile for local coordinate system
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// (x, y) in tile-local coordinates where (0,0) is top-left
#[inline]
pub fn world_to_tile_local(coord: &WorldCoord, tile: &TileCoord, extent: u32) -> (i32, i32) {
    coord.to_tile_local(tile, extent)
}

/// Encode a slice of WorldCoords as an MVT point geometry.
///
/// For a single point, produces MoveTo(1) + zigzag(x) + zigzag(y).
/// For multiple points (MultiPoint), produces MoveTo(n) + delta-encoded pairs.
///
/// # Arguments
/// * `coords` - World coordinates to encode
/// * `tile` - Target tile
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// MVT geometry command stream
pub fn encode_world_points(coords: &[WorldCoord], tile: &TileCoord, extent: u32) -> Vec<u32> {
    if coords.is_empty() {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(1 + coords.len() * 2);
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;

    geometry.push(command_encode(CMD_MOVE_TO, coords.len() as u32));

    for coord in coords {
        let (x, y) = world_to_tile_local(coord, tile, extent);
        let dx = x - cursor_x;
        let dy = y - cursor_y;
        geometry.push(zigzag_encode(dx));
        geometry.push(zigzag_encode(dy));
        cursor_x = x;
        cursor_y = y;
    }

    geometry
}

/// Encode a slice of WorldCoords as an MVT linestring geometry.
///
/// Produces MoveTo(1) for the first point, then LineTo(n-1) for remaining points,
/// all delta-encoded.
///
/// # Arguments
/// * `coords` - World coordinates forming the linestring (must have >= 2 points)
/// * `tile` - Target tile
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// MVT geometry command stream, or empty vec if fewer than 2 points
pub fn encode_world_linestring(coords: &[WorldCoord], tile: &TileCoord, extent: u32) -> Vec<u32> {
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;
    encode_world_linestring_with_cursor(coords, tile, extent, &mut cursor_x, &mut cursor_y)
}

/// Encode a linestring with an external cursor for delta encoding.
///
/// This variant is used for MultiLineString encoding where the cursor must
/// be maintained across multiple linestrings per the MVT spec.
pub fn encode_world_linestring_with_cursor(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    cursor_x: &mut i32,
    cursor_y: &mut i32,
) -> Vec<u32> {
    if coords.len() < 2 {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(3 + (coords.len() - 1) * 2);

    // First point: MoveTo
    let (x, y) = world_to_tile_local(&coords[0], tile, extent);
    let dx = x - *cursor_x;
    let dy = y - *cursor_y;
    geometry.push(command_encode(CMD_MOVE_TO, 1));
    geometry.push(zigzag_encode(dx));
    geometry.push(zigzag_encode(dy));
    *cursor_x = x;
    *cursor_y = y;

    // Remaining points: LineTo
    geometry.push(command_encode(CMD_LINE_TO, (coords.len() - 1) as u32));
    for coord in &coords[1..] {
        let (x, y) = world_to_tile_local(coord, tile, extent);
        let dx = x - *cursor_x;
        let dy = y - *cursor_y;
        geometry.push(zigzag_encode(dx));
        geometry.push(zigzag_encode(dy));
        *cursor_x = x;
        *cursor_y = y;
    }

    geometry
}

/// Encode a ring of WorldCoords as part of an MVT polygon geometry.
///
/// Produces MoveTo(1) for first point, LineTo(n-2) for interior points
/// (skipping the closing point), and ClosePath(1).
///
/// # Arguments
/// * `coords` - World coordinates forming the ring (must have >= 4 points, last == first)
/// * `tile` - Target tile
/// * `extent` - Tile extent (typically 4096)
/// * `cursor_x` - Mutable cursor X position (for delta encoding across rings)
/// * `cursor_y` - Mutable cursor Y position
///
/// # Returns
/// MVT geometry command stream, or empty vec if fewer than 4 points
pub fn encode_world_ring(
    coords: &[WorldCoord],
    tile: &TileCoord,
    extent: u32,
    cursor_x: &mut i32,
    cursor_y: &mut i32,
) -> Vec<u32> {
    // Rings must have at least 4 points (3 unique + closing point)
    if coords.len() < 4 {
        return vec![];
    }

    let mut geometry = Vec::with_capacity(4 + (coords.len() - 2) * 2);

    // First point: MoveTo
    let (x, y) = world_to_tile_local(&coords[0], tile, extent);
    let dx = x - *cursor_x;
    let dy = y - *cursor_y;
    geometry.push(command_encode(CMD_MOVE_TO, 1));
    geometry.push(zigzag_encode(dx));
    geometry.push(zigzag_encode(dy));
    *cursor_x = x;
    *cursor_y = y;

    // Interior points: LineTo (skip last point since ClosePath handles it)
    let line_to_count = coords.len() - 2;
    if line_to_count > 0 {
        geometry.push(command_encode(CMD_LINE_TO, line_to_count as u32));
        for coord in coords.iter().skip(1).take(line_to_count) {
            let (x, y) = world_to_tile_local(coord, tile, extent);
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

/// Encode a polygon from WorldCoord rings as MVT geometry commands.
///
/// # Arguments
/// * `exterior` - Exterior ring coordinates (must have >= 4 points, last == first)
/// * `interiors` - Interior ring (hole) coordinates
/// * `tile` - Target tile
/// * `extent` - Tile extent (typically 4096)
///
/// # Returns
/// MVT geometry command stream
///
/// # Note
/// Callers are responsible for ensuring correct winding order.
/// WorldCoord polygons should already have exterior rings clockwise
/// and interior rings counter-clockwise in tile-local coordinates.
pub fn encode_world_polygon(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    tile: &TileCoord,
    extent: u32,
) -> Vec<u32> {
    let mut cursor_x = 0i32;
    let mut cursor_y = 0i32;
    encode_world_polygon_with_cursor(
        exterior,
        interiors,
        tile,
        extent,
        &mut cursor_x,
        &mut cursor_y,
    )
}

/// Encode a polygon with an external cursor for delta encoding.
///
/// This variant is used for MultiPolygon encoding where the cursor must
/// be maintained across multiple polygons per the MVT spec.
pub fn encode_world_polygon_with_cursor(
    exterior: &[WorldCoord],
    interiors: &[Vec<WorldCoord>],
    tile: &TileCoord,
    extent: u32,
    cursor_x: &mut i32,
    cursor_y: &mut i32,
) -> Vec<u32> {
    let mut geometry = Vec::new();

    // Exterior ring
    let ext_cmds = encode_world_ring(exterior, tile, extent, cursor_x, cursor_y);
    geometry.extend(ext_cmds);

    // Interior rings (holes)
    for interior in interiors {
        let int_cmds = encode_world_ring(interior, tile, extent, cursor_x, cursor_y);
        geometry.extend(int_cmds);
    }

    geometry
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

    /// Add a feature from WorldClippedGeometry directly.
    ///
    /// This method encodes WorldCoord geometries directly without going through
    /// geo::Geometry, avoiding floating-point conversions.
    ///
    /// # Arguments
    /// * `id` - Optional feature ID
    /// * `geometry` - The WorldClippedGeometry to encode
    /// * `properties` - Feature properties as key-value pairs
    /// * `tile` - The tile coordinates for the geometry
    pub fn add_feature_world(
        &mut self,
        id: Option<u64>,
        geometry: &crate::hierarchical_clip::WorldClippedGeometry,
        properties: &[(String, PropertyValue)],
        tile: &TileCoord,
    ) {
        use crate::hierarchical_clip::WorldClippedGeometry;

        let (geom_commands, geom_type) = match geometry {
            WorldClippedGeometry::Point(p) => {
                let cmds = encode_world_points(&[*p], tile, self.extent);
                (cmds, GeomType::Point)
            }
            WorldClippedGeometry::LineString(coords) => {
                let cmds = encode_world_linestring(coords, tile, self.extent);
                if cmds.is_empty() {
                    (vec![], GeomType::Unknown)
                } else {
                    (cmds, GeomType::Linestring)
                }
            }
            WorldClippedGeometry::Polygon {
                exterior,
                interiors,
            } => {
                let cmds = encode_world_polygon(exterior, interiors, tile, self.extent);
                if cmds.is_empty() {
                    (vec![], GeomType::Unknown)
                } else {
                    (cmds, GeomType::Polygon)
                }
            }
            WorldClippedGeometry::MultiPoint(points) => {
                let cmds = encode_world_points(points, tile, self.extent);
                (cmds, GeomType::Point)
            }
            WorldClippedGeometry::MultiLineString(lines) => {
                let mut cmds = Vec::new();
                let mut cursor_x = 0i32;
                let mut cursor_y = 0i32;
                for line in lines {
                    cmds.extend(encode_world_linestring_with_cursor(
                        line,
                        tile,
                        self.extent,
                        &mut cursor_x,
                        &mut cursor_y,
                    ));
                }
                if cmds.is_empty() {
                    (vec![], GeomType::Unknown)
                } else {
                    (cmds, GeomType::Linestring)
                }
            }
            WorldClippedGeometry::MultiPolygon(polys) => {
                let mut cmds = Vec::new();
                let mut cursor_x = 0i32;
                let mut cursor_y = 0i32;
                for (ext, ints) in polys {
                    cmds.extend(encode_world_polygon_with_cursor(
                        ext,
                        ints,
                        tile,
                        self.extent,
                        &mut cursor_x,
                        &mut cursor_y,
                    ));
                }
                if cmds.is_empty() {
                    (vec![], GeomType::Unknown)
                } else {
                    (cmds, GeomType::Polygon)
                }
            }
        };

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
        // A polygon with correct MVT winding (CCW exterior in geographic coords,
        // which becomes CW exterior in tile coords after Y-flip) should pass through unchanged.
        //
        // MVT requires: exterior rings CW, interior rings CCW (in tile/screen coords)
        // Geographic CCW -> Tile CW (after Y-flip), so geo::Direction::Default is correct.

        // CCW polygon in geographic coords (correct for MVT after Y-flip)
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 1.0, y: 0.0),
            (x: 1.0, y: 1.0),
            (x: 0.0, y: 1.0),
            (x: 0.0, y: 0.0),
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Should be unchanged since it's already correctly oriented
        assert_eq!(poly.exterior().0, oriented.exterior().0);
    }

    #[test]
    fn test_polygon_incorrect_winding_gets_corrected() {
        // A polygon with incorrect winding (CW exterior in geographic coords)
        // should be corrected to CCW exterior.

        // CW polygon in geographic coords (incorrect - needs correction)
        let poly = polygon![
            (x: 0.0, y: 0.0),
            (x: 0.0, y: 1.0),
            (x: 1.0, y: 1.0),
            (x: 1.0, y: 0.0),
            (x: 0.0, y: 0.0),
        ];

        let oriented = orient_polygon_for_mvt(&poly);

        // Should now be CCW (reversed from input)
        // The first and last points stay the same, but the middle points should be reversed
        assert_ne!(poly.exterior().0[1], oriented.exterior().0[1]);
    }

    #[test]
    fn test_polygon_with_hole_correct_winding() {
        // A polygon with a hole should have:
        // - CCW exterior in geographic coords (becomes CW in tile coords)
        // - CW interior in geographic coords (becomes CCW in tile coords)

        // Exterior: CCW in geo coords
        // Interior: CW in geo coords (correct for a hole)
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

        // After orientation, exterior should be CCW and interior should be CW
        // (in geographic coordinates, which becomes CW exterior / CCW interior in tile coords)
        assert_eq!(oriented.interiors().len(), 1);
    }

    #[test]
    fn test_polygon_with_hole_incorrect_winding_gets_corrected() {
        // A polygon where both exterior and interior have wrong winding

        // Exterior: CW in geo coords (wrong)
        // Interior: CCW in geo coords (wrong for a hole)
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

    // ------------------------------------------------------------------------
    // WorldCoord Encoding Tests
    // ------------------------------------------------------------------------

    use crate::tile::TileCoord;
    use crate::world_coord::lng_lat_to_world;

    #[test]
    fn test_world_to_tile_local_center_of_world() {
        // Null Island (0, 0) in world coords is (WORLD_HALF, WORLD_HALF)
        // In tile 0/0/0 with extent 4096, this should be (2048, 2048)
        let coord = lng_lat_to_world(0.0, 0.0);
        let tile = TileCoord::new(0, 0, 0);
        let (x, y) = world_to_tile_local(&coord, &tile, 4096);

        assert!(
            (x - 2048).abs() <= 1,
            "Center of world in tile 0/0/0 should be near 2048, got {}",
            x
        );
        assert!(
            (y - 2048).abs() <= 1,
            "Center of world in tile 0/0/0 should be near 2048, got {}",
            y
        );
    }

    #[test]
    fn test_world_to_tile_local_x_matches_f64_path() {
        // X coordinates should match between f64 and WorldCoord paths
        // because longitude is linearly mapped in both cases.
        //
        // DIVERGENCE FROM F64 PATH:
        // Y coordinates differ because geo_to_tile_coords linearly interpolates
        // latitude within TileBounds, while WorldCoord uses the correct Mercator
        // projection. The WorldCoord path is more accurate for Y coordinates.
        let tile = TileCoord::new(0, 0, 1);
        let bounds = tile.bounds();
        let extent = 4096;

        // Center of tile in longitude
        let lng = (bounds.lng_min + bounds.lng_max) / 2.0;
        let lat = (bounds.lat_min + bounds.lat_max) / 2.0;

        // f64 path
        let (f64_x, _f64_y) = geo_to_tile_coords(lng, lat, &bounds, extent);

        // WorldCoord path
        let world = lng_lat_to_world(lng, lat);
        let (wc_x, _wc_y) = world_to_tile_local(&world, &tile, extent);

        // X should match closely (both linear in longitude)
        assert!(
            (f64_x - wc_x).abs() <= 1,
            "X mismatch: f64={} vs WorldCoord={}",
            f64_x,
            wc_x
        );

        // Y values differ because f64 path linearly interpolates latitude
        // while WorldCoord uses Mercator projection. This is expected and
        // WorldCoord is the more correct approach.
    }

    #[test]
    fn test_encode_world_points_single() {
        let tile = TileCoord::new(0, 0, 0);
        let coord = lng_lat_to_world(0.0, 0.0);
        let commands = encode_world_points(&[coord], &tile, 4096);

        // Should be: [MoveTo(1), zigzag(x), zigzag(y)]
        assert_eq!(commands.len(), 3);
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1));
    }

    #[test]
    fn test_encode_world_points_multiple_delta_encoded() {
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![
            lng_lat_to_world(-90.0, 45.0),
            lng_lat_to_world(0.0, 0.0),
            lng_lat_to_world(90.0, -45.0),
        ];
        let commands = encode_world_points(&coords, &tile, 4096);

        // MoveTo(3) + 3 * (dx, dy) = 1 + 6 = 7
        assert_eq!(commands.len(), 7);
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 3));

        // Decode first delta - should be the absolute position of first point
        let first_x = zigzag_decode(commands[1]);
        let first_y = zigzag_decode(commands[2]);
        assert!((0..=4096).contains(&first_x));
        assert!((0..=4096).contains(&first_y));
    }

    #[test]
    fn test_encode_world_points_empty() {
        let tile = TileCoord::new(0, 0, 0);
        let commands = encode_world_points(&[], &tile, 4096);
        assert!(commands.is_empty());
    }

    #[test]
    fn test_encode_world_linestring_simple() {
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![
            lng_lat_to_world(-90.0, 45.0),
            lng_lat_to_world(0.0, 0.0),
            lng_lat_to_world(90.0, -45.0),
        ];
        let commands = encode_world_linestring(&coords, &tile, 4096);

        // MoveTo(1) + 2 coords + LineTo(2) + 4 coords = 8
        assert_eq!(commands.len(), 8);
        assert_eq!(commands[0], command_encode(CMD_MOVE_TO, 1));
        assert_eq!(commands[3], command_encode(CMD_LINE_TO, 2));
    }

    #[test]
    fn test_encode_world_linestring_too_short() {
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![lng_lat_to_world(0.0, 0.0)];
        let commands = encode_world_linestring(&coords, &tile, 4096);
        assert!(commands.is_empty());
    }

    #[test]
    fn test_encode_world_linestring_empty() {
        let tile = TileCoord::new(0, 0, 0);
        let commands = encode_world_linestring(&[], &tile, 4096);
        assert!(commands.is_empty());
    }

    #[test]
    fn test_encode_world_ring_triangle() {
        let tile = TileCoord::new(0, 0, 0);
        // Triangle with closing point: 4 points
        let coords = vec![
            lng_lat_to_world(-90.0, 45.0),
            lng_lat_to_world(0.0, -45.0),
            lng_lat_to_world(90.0, 45.0),
            lng_lat_to_world(-90.0, 45.0), // closing
        ];
        let mut cx = 0i32;
        let mut cy = 0i32;
        let commands = encode_world_ring(&coords, &tile, 4096, &mut cx, &mut cy);

        // 4 coords, line_to_count = 4 - 2 = 2
        // MoveTo(1) + zigzag(dx) + zigzag(dy) + LineTo(2) + 2*(zigzag(dx)+zigzag(dy)) + ClosePath(1)
        // = 1 + 2 + 1 + 4 + 1 = 9
        assert_eq!(commands.len(), 9);
        assert_eq!(command_decode(commands[0]).0, CMD_MOVE_TO);
        assert_eq!(command_decode(*commands.last().unwrap()).0, CMD_CLOSE_PATH);
    }

    #[test]
    fn test_encode_world_ring_too_few_points() {
        let tile = TileCoord::new(0, 0, 0);
        let coords = vec![
            lng_lat_to_world(0.0, 0.0),
            lng_lat_to_world(1.0, 0.0),
            lng_lat_to_world(0.0, 0.0),
        ];
        let mut cx = 0i32;
        let mut cy = 0i32;
        let commands = encode_world_ring(&coords, &tile, 4096, &mut cx, &mut cy);
        assert!(commands.is_empty());
    }

    #[test]
    fn test_encode_world_polygon_simple() {
        let tile = TileCoord::new(0, 0, 0);
        let exterior = vec![
            lng_lat_to_world(-90.0, 45.0),
            lng_lat_to_world(0.0, -45.0),
            lng_lat_to_world(90.0, 45.0),
            lng_lat_to_world(-90.0, 45.0), // closing
        ];
        let commands = encode_world_polygon(&exterior, &[], &tile, 4096);

        assert!(!commands.is_empty());
        assert_eq!(command_decode(commands[0]).0, CMD_MOVE_TO);
        assert_eq!(command_decode(*commands.last().unwrap()).0, CMD_CLOSE_PATH);
    }

    #[test]
    fn test_encode_world_polygon_with_hole() {
        let tile = TileCoord::new(0, 0, 0);

        let exterior = vec![
            lng_lat_to_world(-90.0, 60.0),
            lng_lat_to_world(-90.0, -60.0),
            lng_lat_to_world(90.0, -60.0),
            lng_lat_to_world(90.0, 60.0),
            lng_lat_to_world(-90.0, 60.0), // closing
        ];

        let hole = vec![
            lng_lat_to_world(-45.0, 30.0),
            lng_lat_to_world(-45.0, -30.0),
            lng_lat_to_world(45.0, -30.0),
            lng_lat_to_world(45.0, 30.0),
            lng_lat_to_world(-45.0, 30.0), // closing
        ];

        let commands = encode_world_polygon(&exterior, &[hole], &tile, 4096);

        assert!(!commands.is_empty());

        // Walk the command stream properly to count ClosePath commands.
        // We must skip over coordinate parameters (not interpret them as commands).
        let mut close_count = 0;
        let mut i = 0;
        while i < commands.len() {
            let (cmd_id, count) = command_decode(commands[i]);
            i += 1;
            match cmd_id {
                CMD_MOVE_TO | CMD_LINE_TO => {
                    // Skip count * 2 coordinate values (dx, dy pairs)
                    i += count as usize * 2;
                }
                CMD_CLOSE_PATH => {
                    close_count += 1;
                    // ClosePath has no parameters
                }
                _ => {}
            }
        }

        assert_eq!(
            close_count, 2,
            "Should have 2 ClosePath commands (exterior + hole)"
        );
    }

    #[test]
    fn test_world_coord_encoding_consistency_with_f64() {
        // Core consistency test: encoding the same geographic point through
        // both paths should produce the same MVT commands (within rounding)
        let tile = TileCoord::new(1234, 2345, 14);
        let bounds = tile.bounds();
        let extent = 4096;

        // Pick a point inside the tile
        let lng = (bounds.lng_min + bounds.lng_max) / 2.0;
        let lat = (bounds.lat_min + bounds.lat_max) / 2.0;

        // f64 path
        let f64_commands = encode_point(&point!(x: lng, y: lat), &bounds, extent);

        // WorldCoord path
        let world = lng_lat_to_world(lng, lat);
        let wc_commands = encode_world_points(&[world], &tile, extent);

        // Both should be 3 commands: MoveTo(1), zigzag(x), zigzag(y)
        assert_eq!(f64_commands.len(), 3);
        assert_eq!(wc_commands.len(), 3);

        // MoveTo command should be identical
        assert_eq!(f64_commands[0], wc_commands[0]);

        // Coordinates should be within 1 unit (rounding difference between paths)
        let f64_x = zigzag_decode(f64_commands[1]);
        let wc_x = zigzag_decode(wc_commands[1]);
        let f64_y = zigzag_decode(f64_commands[2]);
        let wc_y = zigzag_decode(wc_commands[2]);

        assert!(
            (f64_x - wc_x).abs() <= 1,
            "X mismatch: f64={} vs WorldCoord={}",
            f64_x,
            wc_x
        );
        assert!(
            (f64_y - wc_y).abs() <= 1,
            "Y mismatch: f64={} vs WorldCoord={}",
            f64_y,
            wc_y
        );
    }

    #[test]
    fn test_world_coord_linestring_consistency_with_f64() {
        // Test that encoding a linestring through both paths gives similar structure.
        //
        // DIVERGENCE FROM F64 PATH:
        // The f64 path (geo_to_tile_coords) linearly interpolates latitude in TileBounds,
        // while WorldCoord uses the Mercator projection. X coordinates (longitude) match
        // closely, but Y coordinates differ because Mercator is non-linear in latitude.
        // The WorldCoord path is more accurate.
        let tile = TileCoord::new(10, 10, 5);
        let bounds = tile.bounds();
        let extent = 4096;

        // Points inside tile
        let lng1 = bounds.lng_min + (bounds.lng_max - bounds.lng_min) * 0.25;
        let lat1 = bounds.lat_min + (bounds.lat_max - bounds.lat_min) * 0.25;
        let lng2 = bounds.lng_min + (bounds.lng_max - bounds.lng_min) * 0.75;
        let lat2 = bounds.lat_min + (bounds.lat_max - bounds.lat_min) * 0.75;

        // f64 path
        let line = line_string![(x: lng1, y: lat1), (x: lng2, y: lat2)];
        let f64_commands = encode_linestring(&line, &bounds, extent);

        // WorldCoord path
        let world_coords = vec![lng_lat_to_world(lng1, lat1), lng_lat_to_world(lng2, lat2)];
        let wc_commands = encode_world_linestring(&world_coords, &tile, extent);

        // Both should have same structure: MoveTo(1) + 2 coords + LineTo(1) + 2 coords = 6
        assert_eq!(f64_commands.len(), wc_commands.len());

        // Command structure should match exactly
        assert_eq!(f64_commands[0], wc_commands[0]); // MoveTo(1)
        assert_eq!(f64_commands[3], wc_commands[3]); // LineTo(1)

        // X coordinates (indices 1, 4) should be close (both linear in longitude)
        for i in [1, 4] {
            let f64_val = zigzag_decode(f64_commands[i]);
            let wc_val = zigzag_decode(wc_commands[i]);
            assert!(
                (f64_val - wc_val).abs() <= 2,
                "X command[{}] mismatch: f64={} vs WorldCoord={}",
                i,
                f64_val,
                wc_val
            );
        }

        // Y coordinates (indices 2, 5) may differ due to Mercator vs linear interpolation
        // Just verify they are within valid tile extent range
        for i in [2, 5] {
            let wc_val = zigzag_decode(wc_commands[i]);
            // For the first Y it's absolute, for delta it can be negative
            // Just verify the values are reasonable (not overflowing)
            assert!(
                wc_val.abs() <= extent as i32 * 2,
                "Y command[{}] out of range: {}",
                i,
                wc_val
            );
        }
    }

    #[test]
    fn test_world_coord_cursor_state_across_rings() {
        // Verify that cursor state is correctly maintained across multiple rings
        let tile = TileCoord::new(0, 0, 0);
        let extent = 4096;

        let ring1 = vec![
            lng_lat_to_world(-90.0, 45.0),
            lng_lat_to_world(-90.0, -45.0),
            lng_lat_to_world(0.0, -45.0),
            lng_lat_to_world(-90.0, 45.0), // closing
        ];

        let ring2 = vec![
            lng_lat_to_world(0.0, 45.0),
            lng_lat_to_world(0.0, -45.0),
            lng_lat_to_world(90.0, -45.0),
            lng_lat_to_world(0.0, 45.0), // closing
        ];

        let mut cx = 0i32;
        let mut cy = 0i32;
        let cmds1 = encode_world_ring(&ring1, &tile, extent, &mut cx, &mut cy);
        let cmds2 = encode_world_ring(&ring2, &tile, extent, &mut cx, &mut cy);

        // Both rings should have produced commands
        assert!(!cmds1.is_empty());
        assert!(!cmds2.is_empty());

        // The second ring's first delta should not be from (0,0) - it should be from
        // where the cursor ended after the first ring
        // (This verifies cursor state is carried across rings)
        let ring2_dx = zigzag_decode(cmds2[1]);
        let ring2_dy = zigzag_decode(cmds2[2]);

        // If cursor state wasn't maintained, dx/dy would be the absolute position
        // of the first point of ring2. Instead, it should be the delta from ring1's
        // last cursor position.
        let (ring2_first_x, ring2_first_y) = world_to_tile_local(&ring2[0], &tile, extent);

        // The delta encoding means these values should differ from the absolute position
        // (unless the cursor happened to be at origin, which it shouldn't be after ring1)
        assert!(
            ring2_dx != ring2_first_x || ring2_dy != ring2_first_y,
            "Ring2's first delta should be relative to ring1's last cursor position, \
             not absolute. dx={}, dy={}, abs_x={}, abs_y={}",
            ring2_dx,
            ring2_dy,
            ring2_first_x,
            ring2_first_y
        );
    }

    // ------------------------------------------------------------------------
    // MVT Encoding Overhead Tests
    // ------------------------------------------------------------------------

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
