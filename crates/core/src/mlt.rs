//! MLT (MapLibre Tiles) encoding module.
//!
//! This module implements the MLT specification for encoding geometries into
//! compact vector tiles. MLT provides better compression than MVT for many use cases.
//!
//! Key encoding techniques:
//! - **Varint encoding**: Efficient integer compression
//! - **Zigzag encoding**: Efficient signed integer representation
//! - **Delta encoding**: Coordinates stored as differences from previous
//! - **RLE encoding**: Run-length encoding for repeated values
//! - **Dictionary encoding**: String deduplication for low-cardinality columns
//!
//! ## Attribution
//!
//! This encoder is adapted from [freestiler](https://github.com/walkerke/freestiler)
//! by Kyle Walker, licensed under the MIT License. See NOTICE.md for details.
//!
//! ## Reference
//!
//! - MLT Specification: <https://github.com/maplibre/maplibre-tile-spec>

use crate::tile::TileCoord;
use geo::{Coord, Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use integer_encoding::VarInt;
use std::collections::HashMap;

// ============================================================================
// Constants
// ============================================================================

/// MLT tile extent (4096 as per specification)
pub const EXTENT: u32 = 4096;

/// MLT layer tag for v01 format
const TAG_V01: u8 = 0x01;

// Column type codes
const COL_ID: u8 = 2; // LongId - 64-bit unsigned IDs
const COL_GEOMETRY: u8 = 4;
const COL_I64: u8 = 20;
const COL_OPT_I64: u8 = 21;
const COL_F64: u8 = 26;
const COL_OPT_F64: u8 = 27;
const COL_STR: u8 = 28;
const COL_OPT_STR: u8 = 29;
const COL_BOOL: u8 = 10;
const COL_OPT_BOOL: u8 = 11;

// Geometry types
const GEOM_POINT: u8 = 0;
const GEOM_LINESTRING: u8 = 1;
const GEOM_POLYGON: u8 = 2;
const GEOM_MULTI_POINT: u8 = 3;
const GEOM_MULTI_LINESTRING: u8 = 4;
const GEOM_MULTI_POLYGON: u8 = 5;

// PhysicalStreamType ordinals (upper nibble of byte 0)
const STREAM_PRESENT: u8 = 0;
const STREAM_DATA: u8 = 1;
const STREAM_OFFSET: u8 = 2;
const STREAM_LENGTH: u8 = 3;

// DictionaryType ordinals (lower nibble of byte 0, when stream type = DATA)
const DATA_NONE: u8 = 0;
const DATA_SINGLE: u8 = 1;
const DATA_VERTEX: u8 = 3;

// OffsetType ordinals (lower nibble of byte 0, when stream type = OFFSET)
const OFFSET_STRING: u8 = 2;

// LengthType ordinals (lower nibble of byte 0, when stream type = LENGTH)
const LENGTH_VAR_BINARY: u8 = 0;
const LENGTH_GEOMETRIES: u8 = 1;
const LENGTH_PARTS: u8 = 2;
const LENGTH_RINGS: u8 = 3;
const LENGTH_DICTIONARY: u8 = 6;

// LogicalLevelTechnique ordinals
const LOG_NONE: u8 = 0;
const LOG_DELTA: u8 = 1;
const LOG_COMPONENTWISE_DELTA: u8 = 2;
const LOG_RLE: u8 = 3;

// PhysicalLevelTechnique ordinals
const PHYS_NONE: u8 = 0;
const PHYS_VARINT: u8 = 2;

// ============================================================================
// Public Types
// ============================================================================

/// A property value that can be encoded in MLT.
#[derive(Debug, Clone, PartialEq)]
pub enum MltPropertyValue {
    /// String value
    String(String),
    /// 64-bit signed integer
    Int(i64),
    /// 64-bit floating point
    Double(f64),
    /// Boolean value
    Bool(bool),
    /// Null/missing value
    Null,
}

impl From<&crate::mvt::PropertyValue> for MltPropertyValue {
    fn from(value: &crate::mvt::PropertyValue) -> Self {
        match value {
            crate::mvt::PropertyValue::String(s) => MltPropertyValue::String(s.clone()),
            crate::mvt::PropertyValue::Float(f) => MltPropertyValue::Double(*f as f64),
            crate::mvt::PropertyValue::Double(d) => MltPropertyValue::Double(*d),
            crate::mvt::PropertyValue::Int(i) => MltPropertyValue::Int(*i),
            crate::mvt::PropertyValue::UInt(u) => MltPropertyValue::Int(*u as i64),
            crate::mvt::PropertyValue::Bool(b) => MltPropertyValue::Bool(*b),
        }
    }
}

/// A feature to be encoded in an MLT tile.
#[derive(Debug, Clone)]
pub struct MltFeature {
    /// Optional feature ID
    pub id: Option<u64>,
    /// The geometry of the feature
    pub geometry: MltGeometry,
    /// Properties as a list of values (ordered by column)
    pub properties: Vec<MltPropertyValue>,
}

/// Geometry types supported by MLT.
#[derive(Debug, Clone)]
pub enum MltGeometry {
    Point(Point),
    MultiPoint(MultiPoint),
    LineString(LineString),
    MultiLineString(MultiLineString),
    Polygon(Polygon),
    MultiPolygon(MultiPolygon),
}

impl From<&Geometry> for MltGeometry {
    fn from(geom: &Geometry) -> Self {
        match geom {
            Geometry::Point(p) => MltGeometry::Point(*p),
            Geometry::MultiPoint(mp) => MltGeometry::MultiPoint(mp.clone()),
            Geometry::LineString(ls) => MltGeometry::LineString(ls.clone()),
            Geometry::MultiLineString(mls) => MltGeometry::MultiLineString(mls.clone()),
            Geometry::Polygon(poly) => MltGeometry::Polygon(poly.clone()),
            Geometry::MultiPolygon(mp) => MltGeometry::MultiPolygon(mp.clone()),
            // For unsupported types, convert to empty point
            _ => MltGeometry::Point(Point::new(0.0, 0.0)),
        }
    }
}

// ============================================================================
// Public API
// ============================================================================

/// Encode a tile into MLT format.
///
/// # Arguments
/// * `coord` - The tile coordinates
/// * `features` - Features to encode
/// * `layer_name` - Name of the layer
/// * `property_names` - Names of the property columns (in order)
///
/// # Returns
/// Encoded MLT tile bytes
pub fn encode_tile_mlt(
    coord: &TileCoord,
    features: &[MltFeature],
    layer_name: &str,
    property_names: &[String],
) -> Vec<u8> {
    encode_tile(coord, features, layer_name, property_names)
}

/// Encode multiple layers into a single MLT tile.
///
/// # Arguments
/// * `coord` - The tile coordinates
/// * `layer_data` - List of (layer_name, property_names, features) tuples
///
/// # Returns
/// Encoded MLT tile bytes
pub fn encode_tile_mlt_multilayer(
    coord: &TileCoord,
    layer_data: &[(&str, &[String], &[MltFeature])],
) -> Vec<u8> {
    let mut tile_bytes = Vec::new();
    for &(layer_name, property_names, features) in layer_data {
        if !features.is_empty() {
            let layer_bytes = encode_tile(coord, features, layer_name, property_names);
            tile_bytes.extend(&layer_bytes);
        }
    }
    tile_bytes
}

// ============================================================================
// Core Encoding Functions
// ============================================================================

/// Encode features into an MLT tile (single layer).
fn encode_tile(
    coord: &TileCoord,
    features: &[MltFeature],
    layer_name: &str,
    property_names: &[String],
) -> Vec<u8> {
    if features.is_empty() {
        return Vec::new();
    }

    let bounds = coord.bounds();
    let west = bounds.lng_min;
    let south = bounds.lat_min;
    let east = bounds.lng_max;
    let north = bounds.lat_max;

    // Build the layer payload
    let mut layer_data = Vec::new();

    // Layer name (varint-prefixed UTF-8)
    write_string(&mut layer_data, layer_name);

    // Extent
    write_varint_u32(&mut layer_data, EXTENT);

    // Count columns: id + geometry + properties
    let num_columns = 2 + property_names.len();
    write_varint_usize(&mut layer_data, num_columns);

    // Column metadata
    // 1. ID column (type code 2 = LongId)
    layer_data.push(COL_ID);
    // 2. Geometry column (type code 4)
    layer_data.push(COL_GEOMETRY);
    // 3. Property columns
    for (i, name) in property_names.iter().enumerate() {
        let col_type = infer_column_type(features, i);
        layer_data.push(col_type);
        write_string(&mut layer_data, name);
    }

    // --- ID stream (delta-encoded unsigned varints) ---
    {
        let ids: Vec<u64> = features.iter().map(|f| f.id.unwrap_or(0)).collect();
        let mut deltas = Vec::with_capacity(ids.len());
        let mut prev = 0u64;
        for &id in &ids {
            deltas.push(id.wrapping_sub(prev));
            prev = id;
        }
        let id_bytes = encode_varint_u64_stream(&deltas);
        write_stream_meta(
            &mut layer_data,
            STREAM_DATA,
            DATA_NONE,
            LOG_DELTA,
            LOG_NONE,
            PHYS_VARINT,
            ids.len(),
            id_bytes.len(),
        );
        layer_data.extend(&id_bytes);
    }

    // --- Geometry streams ---
    let geom_stream_count = count_geometry_streams(features);
    write_varint_usize(&mut layer_data, geom_stream_count);
    encode_geometry_streams(&mut layer_data, features, west, south, east, north);

    // --- Property streams ---
    for (i, _name) in property_names.iter().enumerate() {
        let col_type = infer_column_type(features, i);
        // STRING columns need a stream count varint
        if col_type == COL_STR || col_type == COL_OPT_STR {
            let has_nulls = features.iter().any(|f| {
                i >= f.properties.len() || matches!(f.properties[i], MltPropertyValue::Null)
            });
            let use_dict = should_use_dictionary(features, i);
            let encoding_streams: usize = if use_dict { 3 } else { 2 };
            let stream_count = if has_nulls {
                encoding_streams + 1
            } else {
                encoding_streams
            };
            write_varint_usize(&mut layer_data, stream_count);
        }
        encode_property_stream(&mut layer_data, features, i);
    }

    // Wrap in layer envelope: varint(length) + varint(tag=1) + layer_data
    let mut tile_bytes = Vec::new();
    let mut tag_buf = [0u8; 5];
    let tag_len = (TAG_V01 as u32).encode_var(&mut tag_buf);
    let total_size = tag_len + layer_data.len();
    write_varint_usize(&mut tile_bytes, total_size);
    tile_bytes.extend_from_slice(&tag_buf[..tag_len]);
    tile_bytes.extend(&layer_data);

    tile_bytes
}

// ============================================================================
// Column Type Inference
// ============================================================================

fn infer_column_type(features: &[MltFeature], prop_idx: usize) -> u8 {
    let mut has_null = false;
    let mut has_string = false;
    let mut has_int = false;
    let mut has_double = false;
    let mut has_bool = false;

    for f in features {
        if prop_idx < f.properties.len() {
            match &f.properties[prop_idx] {
                MltPropertyValue::Null => has_null = true,
                MltPropertyValue::String(_) => has_string = true,
                MltPropertyValue::Int(_) => has_int = true,
                MltPropertyValue::Double(_) => has_double = true,
                MltPropertyValue::Bool(_) => has_bool = true,
            }
        } else {
            has_null = true;
        }
    }

    // Priority: string > double > int > bool
    if has_string {
        if has_null {
            COL_OPT_STR
        } else {
            COL_STR
        }
    } else if has_double {
        if has_null {
            COL_OPT_F64
        } else {
            COL_F64
        }
    } else if has_int {
        if has_null {
            COL_OPT_I64
        } else {
            COL_I64
        }
    } else if has_bool {
        if has_null {
            COL_OPT_BOOL
        } else {
            COL_BOOL
        }
    } else {
        COL_OPT_STR // all nulls
    }
}

// ============================================================================
// Geometry Encoding
// ============================================================================

fn count_geometry_streams(features: &[MltFeature]) -> usize {
    let mut has_multi = false;
    let mut has_parts = false;
    let mut has_rings = false;

    for f in features {
        match &f.geometry {
            MltGeometry::Point(_) => {}
            MltGeometry::MultiPoint(_) => has_multi = true,
            MltGeometry::LineString(_) => has_parts = true,
            MltGeometry::MultiLineString(_) => {
                has_multi = true;
                has_parts = true;
            }
            MltGeometry::Polygon(_) => {
                has_parts = true;
                has_rings = true;
            }
            MltGeometry::MultiPolygon(_) => {
                has_multi = true;
                has_parts = true;
                has_rings = true;
            }
        }
    }

    let mut count = 2; // geom_type + vertex
    if has_multi {
        count += 1;
    }
    if has_parts {
        count += 1;
    }
    if has_rings {
        count += 1;
    }
    count
}

fn encode_geometry_streams(
    out: &mut Vec<u8>,
    features: &[MltFeature],
    west: f64,
    south: f64,
    east: f64,
    north: f64,
) {
    let n = features.len();

    // 1. Geometry type stream
    let geom_types: Vec<u32> = features
        .iter()
        .map(|f| geometry_type_byte(&f.geometry) as u32)
        .collect();
    let geom_type_runs = count_runs(&geom_types);
    if geom_type_runs * 2 < geom_types.len() {
        let (rle_bytes, num_runs, num_rle_values) = integer_rle_encode_u32(&geom_types);
        write_stream_meta_rle(
            out,
            STREAM_LENGTH,
            LENGTH_VAR_BINARY,
            LOG_RLE,
            LOG_NONE,
            PHYS_VARINT,
            num_runs * 2,
            rle_bytes.len(),
            num_runs,
            num_rle_values,
        );
        out.extend(&rle_bytes);
    } else {
        let bytes = encode_varint_u32_stream(&geom_types);
        write_stream_meta(
            out,
            STREAM_LENGTH,
            LENGTH_VAR_BINARY,
            LOG_NONE,
            LOG_NONE,
            PHYS_VARINT,
            n,
            bytes.len(),
        );
        out.extend(&bytes);
    }

    // Collect topology and vertex data
    let mut num_geometries: Vec<u32> = Vec::new();
    let mut num_parts: Vec<u32> = Vec::new();
    let mut num_rings: Vec<u32> = Vec::new();
    let mut vertices_x: Vec<i32> = Vec::new();
    let mut vertices_y: Vec<i32> = Vec::new();

    for feature in features {
        collect_geometry_data(
            &feature.geometry,
            west,
            south,
            east,
            north,
            &mut num_geometries,
            &mut num_parts,
            &mut num_rings,
            &mut vertices_x,
            &mut vertices_y,
        );
    }

    // 2. NumGeometries stream
    if !num_geometries.is_empty() {
        let runs = count_runs(&num_geometries);
        if runs * 2 < num_geometries.len() {
            let (rle_bytes, num_runs, num_rle_values) = integer_rle_encode_u32(&num_geometries);
            write_stream_meta_rle(
                out,
                STREAM_LENGTH,
                LENGTH_GEOMETRIES,
                LOG_RLE,
                LOG_NONE,
                PHYS_VARINT,
                num_runs * 2,
                rle_bytes.len(),
                num_runs,
                num_rle_values,
            );
            out.extend(&rle_bytes);
        } else {
            let bytes = encode_varint_u32_stream(&num_geometries);
            write_stream_meta(
                out,
                STREAM_LENGTH,
                LENGTH_GEOMETRIES,
                LOG_NONE,
                LOG_NONE,
                PHYS_VARINT,
                num_geometries.len(),
                bytes.len(),
            );
            out.extend(&bytes);
        }
    }

    // 3. NumParts stream
    if !num_parts.is_empty() {
        let runs = count_runs(&num_parts);
        if runs * 2 < num_parts.len() {
            let (rle_bytes, num_runs, num_rle_values) = integer_rle_encode_u32(&num_parts);
            write_stream_meta_rle(
                out,
                STREAM_LENGTH,
                LENGTH_PARTS,
                LOG_RLE,
                LOG_NONE,
                PHYS_VARINT,
                num_runs * 2,
                rle_bytes.len(),
                num_runs,
                num_rle_values,
            );
            out.extend(&rle_bytes);
        } else {
            let bytes = encode_varint_u32_stream(&num_parts);
            write_stream_meta(
                out,
                STREAM_LENGTH,
                LENGTH_PARTS,
                LOG_NONE,
                LOG_NONE,
                PHYS_VARINT,
                num_parts.len(),
                bytes.len(),
            );
            out.extend(&bytes);
        }
    }

    // 4. NumRings stream
    if !num_rings.is_empty() {
        let runs = count_runs(&num_rings);
        if runs * 2 < num_rings.len() {
            let (rle_bytes, num_runs, num_rle_values) = integer_rle_encode_u32(&num_rings);
            write_stream_meta_rle(
                out,
                STREAM_LENGTH,
                LENGTH_RINGS,
                LOG_RLE,
                LOG_NONE,
                PHYS_VARINT,
                num_runs * 2,
                rle_bytes.len(),
                num_runs,
                num_rle_values,
            );
            out.extend(&rle_bytes);
        } else {
            let bytes = encode_varint_u32_stream(&num_rings);
            write_stream_meta(
                out,
                STREAM_LENGTH,
                LENGTH_RINGS,
                LOG_NONE,
                LOG_NONE,
                PHYS_VARINT,
                num_rings.len(),
                bytes.len(),
            );
            out.extend(&bytes);
        }
    }

    // 5. Vertex buffer - interleaved x, y with componentwise delta
    if !vertices_x.is_empty() {
        let total_vertices = vertices_x.len();
        let dx = delta_encode_i32(&vertices_x);
        let dy = delta_encode_i32(&vertices_y);
        let mut interleaved_zigzag = Vec::with_capacity(dx.len() + dy.len());
        for i in 0..dx.len() {
            interleaved_zigzag.push(zigzag_encode_i32(dx[i]));
            interleaved_zigzag.push(zigzag_encode_i32(dy[i]));
        }
        let bytes = encode_varint_u32_stream(&interleaved_zigzag);
        write_stream_meta(
            out,
            STREAM_DATA,
            DATA_VERTEX,
            LOG_COMPONENTWISE_DELTA,
            LOG_NONE,
            PHYS_VARINT,
            total_vertices * 2,
            bytes.len(),
        );
        out.extend(&bytes);
    }
}

fn geometry_type_byte(geom: &MltGeometry) -> u8 {
    match geom {
        MltGeometry::Point(_) => GEOM_POINT,
        MltGeometry::MultiPoint(_) => GEOM_MULTI_POINT,
        MltGeometry::LineString(_) => GEOM_LINESTRING,
        MltGeometry::MultiLineString(_) => GEOM_MULTI_LINESTRING,
        MltGeometry::Polygon(_) => GEOM_POLYGON,
        MltGeometry::MultiPolygon(_) => GEOM_MULTI_POLYGON,
    }
}

fn collect_geometry_data(
    geom: &MltGeometry,
    west: f64,
    south: f64,
    east: f64,
    north: f64,
    num_geometries: &mut Vec<u32>,
    num_parts: &mut Vec<u32>,
    num_rings: &mut Vec<u32>,
    vertices_x: &mut Vec<i32>,
    vertices_y: &mut Vec<i32>,
) {
    match geom {
        MltGeometry::Point(p) => {
            let x = lon_to_tile_coord(p.x(), west, east);
            let y = lat_to_tile_coord(p.y(), south, north);
            vertices_x.push(x);
            vertices_y.push(y);
        }
        MltGeometry::MultiPoint(mp) => {
            num_geometries.push(mp.0.len() as u32);
            for p in &mp.0 {
                let x = lon_to_tile_coord(p.x(), west, east);
                let y = lat_to_tile_coord(p.y(), south, north);
                vertices_x.push(x);
                vertices_y.push(y);
            }
        }
        MltGeometry::LineString(ls) => {
            num_parts.push(ls.0.len() as u32);
            for c in &ls.0 {
                vertices_x.push(lon_to_tile_coord(c.x, west, east));
                vertices_y.push(lat_to_tile_coord(c.y, south, north));
            }
        }
        MltGeometry::MultiLineString(mls) => {
            num_geometries.push(mls.0.len() as u32);
            for ls in &mls.0 {
                num_parts.push(ls.0.len() as u32);
                for c in &ls.0 {
                    vertices_x.push(lon_to_tile_coord(c.x, west, east));
                    vertices_y.push(lat_to_tile_coord(c.y, south, north));
                }
            }
        }
        MltGeometry::Polygon(poly) => {
            let ring_count = 1 + poly.interiors().len();
            num_parts.push(ring_count as u32);
            // Exterior ring (skip closing point)
            let ext = poly.exterior();
            let ext_coords: Vec<&Coord> = if ext.0.len() >= 2 && ext.0.first() == ext.0.last() {
                ext.0[..ext.0.len() - 1].iter().collect()
            } else {
                ext.0.iter().collect()
            };
            num_rings.push(ext_coords.len() as u32);
            for c in &ext_coords {
                vertices_x.push(lon_to_tile_coord(c.x, west, east));
                vertices_y.push(lat_to_tile_coord(c.y, south, north));
            }
            // Interior rings
            for interior in poly.interiors() {
                let int_coords: Vec<&Coord> =
                    if interior.0.len() >= 2 && interior.0.first() == interior.0.last() {
                        interior.0[..interior.0.len() - 1].iter().collect()
                    } else {
                        interior.0.iter().collect()
                    };
                num_rings.push(int_coords.len() as u32);
                for c in &int_coords {
                    vertices_x.push(lon_to_tile_coord(c.x, west, east));
                    vertices_y.push(lat_to_tile_coord(c.y, south, north));
                }
            }
        }
        MltGeometry::MultiPolygon(mp) => {
            num_geometries.push(mp.0.len() as u32);
            for poly in &mp.0 {
                let ring_count = 1 + poly.interiors().len();
                num_parts.push(ring_count as u32);
                let ext = poly.exterior();
                let ext_coords: Vec<&Coord> = if ext.0.len() >= 2 && ext.0.first() == ext.0.last() {
                    ext.0[..ext.0.len() - 1].iter().collect()
                } else {
                    ext.0.iter().collect()
                };
                num_rings.push(ext_coords.len() as u32);
                for c in &ext_coords {
                    vertices_x.push(lon_to_tile_coord(c.x, west, east));
                    vertices_y.push(lat_to_tile_coord(c.y, south, north));
                }
                for interior in poly.interiors() {
                    let int_coords: Vec<&Coord> =
                        if interior.0.len() >= 2 && interior.0.first() == interior.0.last() {
                            interior.0[..interior.0.len() - 1].iter().collect()
                        } else {
                            interior.0.iter().collect()
                        };
                    num_rings.push(int_coords.len() as u32);
                    for c in &int_coords {
                        vertices_x.push(lon_to_tile_coord(c.x, west, east));
                        vertices_y.push(lat_to_tile_coord(c.y, south, north));
                    }
                }
            }
        }
    }
}

// ============================================================================
// Property Encoding
// ============================================================================

fn property_value_as_string(value: &MltPropertyValue) -> Option<String> {
    match value {
        MltPropertyValue::String(s) => Some(s.clone()),
        MltPropertyValue::Int(v) => Some(v.to_string()),
        MltPropertyValue::Double(v) => Some(v.to_string()),
        MltPropertyValue::Bool(v) => Some(v.to_string()),
        MltPropertyValue::Null => None,
    }
}

fn collect_string_values(features: &[MltFeature], prop_idx: usize) -> Vec<String> {
    features
        .iter()
        .filter_map(|f| {
            let value = f
                .properties
                .get(prop_idx)
                .unwrap_or(&MltPropertyValue::Null);
            property_value_as_string(value)
        })
        .collect()
}

fn should_use_dictionary(features: &[MltFeature], prop_idx: usize) -> bool {
    let col_type = infer_column_type(features, prop_idx);
    if col_type != COL_STR && col_type != COL_OPT_STR {
        return false;
    }

    let string_values = collect_string_values(features, prop_idx);
    if string_values.is_empty() {
        return false;
    }

    // Raw cost estimate
    let raw_len_bytes: usize = string_values
        .iter()
        .map(|s| {
            let mut buf = [0u8; 5];
            (s.len() as u32).encode_var(&mut buf)
        })
        .sum();
    let raw_data_bytes: usize = string_values.iter().map(|s| s.len()).sum();
    let raw_cost = raw_len_bytes + raw_data_bytes + 8;

    // Dictionary cost estimate
    let mut unique_map: HashMap<&str, u32> = HashMap::new();
    let mut dict_entries: Vec<&str> = Vec::new();
    for s in &string_values {
        if !unique_map.contains_key(s.as_str()) {
            let idx = dict_entries.len() as u32;
            unique_map.insert(s.as_str(), idx);
            dict_entries.push(s.as_str());
        }
    }

    if dict_entries.len() >= string_values.len() {
        return false; // All unique, no savings
    }

    let dict_data_bytes: usize = dict_entries.iter().map(|s| s.len()).sum();
    let dict_len_bytes: usize = dict_entries
        .iter()
        .map(|s| {
            let mut buf = [0u8; 5];
            (s.len() as u32).encode_var(&mut buf)
        })
        .sum();
    let index_bytes: usize = string_values
        .iter()
        .map(|s| {
            let mut buf = [0u8; 5];
            unique_map[s.as_str()].encode_var(&mut buf)
        })
        .sum();
    let dict_cost = dict_len_bytes + dict_data_bytes + index_bytes + 12;

    dict_cost < raw_cost
}

fn encode_property_stream(out: &mut Vec<u8>, features: &[MltFeature], prop_idx: usize) {
    let n = features.len();

    // Check for nulls
    let has_nulls = features.iter().any(|f| {
        prop_idx >= f.properties.len() || matches!(f.properties[prop_idx], MltPropertyValue::Null)
    });

    // Write presence bitmap if needed (byte-RLE encoded)
    if has_nulls {
        let mut bitmap = Vec::new();
        let mut byte: u8 = 0;
        for (i, f) in features.iter().enumerate() {
            let present = prop_idx < f.properties.len()
                && !matches!(f.properties[prop_idx], MltPropertyValue::Null);
            if present {
                byte |= 1 << (i % 8);
            }
            if i % 8 == 7 || i == n - 1 {
                bitmap.push(byte);
                byte = 0;
            }
        }
        let rle_data = byte_rle_encode(&bitmap);
        write_stream_meta(
            out,
            STREAM_PRESENT,
            0,
            LOG_RLE,
            LOG_NONE,
            PHYS_NONE,
            n,
            rle_data.len(),
        );
        out.extend(&rle_data);
    }

    let col_type = infer_column_type(features, prop_idx);
    match col_type {
        COL_STR | COL_OPT_STR => {
            let string_values = collect_string_values(features, prop_idx);

            // Calculate costs
            let raw_lengths: Vec<u32> = string_values.iter().map(|s| s.len() as u32).collect();
            let raw_len_bytes = encode_varint_u32_stream(&raw_lengths);
            let raw_data_bytes: usize = string_values.iter().map(|s| s.len()).sum();
            let raw_cost = raw_len_bytes.len() + raw_data_bytes + 8;

            // Dictionary encoding
            let mut unique_map: HashMap<&str, u32> = HashMap::new();
            let mut dict_entries: Vec<&str> = Vec::new();
            for s in &string_values {
                if !unique_map.contains_key(s.as_str()) {
                    let idx = dict_entries.len() as u32;
                    unique_map.insert(s.as_str(), idx);
                    dict_entries.push(s.as_str());
                }
            }

            let dict_data_bytes: usize = dict_entries.iter().map(|s| s.len()).sum();
            let dict_lengths: Vec<u32> = dict_entries.iter().map(|s| s.len() as u32).collect();
            let dict_len_encoded = encode_varint_u32_stream(&dict_lengths);
            let indices: Vec<u32> = string_values
                .iter()
                .map(|s| unique_map[s.as_str()])
                .collect();
            let index_encoded = encode_varint_u32_stream(&indices);
            let dict_cost = dict_len_encoded.len() + dict_data_bytes + index_encoded.len() + 12;

            if dict_cost < raw_cost && dict_entries.len() < string_values.len() {
                // Dictionary encoding wins
                encode_dictionary_streams(
                    out,
                    &dict_entries,
                    &dict_lengths,
                    dict_data_bytes,
                    &indices,
                );
            } else {
                // Raw encoding
                write_stream_meta(
                    out,
                    STREAM_LENGTH,
                    LENGTH_VAR_BINARY,
                    LOG_NONE,
                    LOG_NONE,
                    PHYS_VARINT,
                    raw_lengths.len(),
                    raw_len_bytes.len(),
                );
                out.extend(&raw_len_bytes);
                write_stream_meta(
                    out,
                    STREAM_DATA,
                    DATA_NONE,
                    LOG_NONE,
                    LOG_NONE,
                    PHYS_NONE,
                    string_values.len(),
                    raw_data_bytes,
                );
                let mut raw_string_data = Vec::with_capacity(raw_data_bytes);
                for s in &string_values {
                    raw_string_data.extend(s.as_bytes());
                }
                out.extend(&raw_string_data);
            }
        }
        COL_I64 | COL_OPT_I64 => {
            let vals: Vec<i64> = features
                .iter()
                .filter_map(|f| {
                    if prop_idx < f.properties.len() {
                        match &f.properties[prop_idx] {
                            MltPropertyValue::Int(i) => Some(*i),
                            MltPropertyValue::Double(d) => Some(*d as i64),
                            MltPropertyValue::Bool(b) => Some(if *b { 1 } else { 0 }),
                            _ => None,
                        }
                    } else {
                        None
                    }
                })
                .collect();
            let bytes = encode_zigzag_varint_i64_stream(&vals);
            write_stream_meta(
                out,
                STREAM_DATA,
                DATA_NONE,
                LOG_NONE,
                LOG_NONE,
                PHYS_VARINT,
                vals.len(),
                bytes.len(),
            );
            out.extend(&bytes);
        }
        COL_F64 | COL_OPT_F64 => {
            let vals: Vec<f64> = features
                .iter()
                .filter_map(|f| {
                    if prop_idx < f.properties.len() {
                        match &f.properties[prop_idx] {
                            MltPropertyValue::Double(d) => Some(*d),
                            MltPropertyValue::Int(i) => Some(*i as f64),
                            _ => None,
                        }
                    } else {
                        None
                    }
                })
                .collect();
            // Write as little-endian f64 bytes
            let mut bytes = Vec::with_capacity(vals.len() * 8);
            for v in &vals {
                bytes.extend(&v.to_le_bytes());
            }
            write_stream_meta(
                out,
                STREAM_DATA,
                DATA_NONE,
                LOG_NONE,
                LOG_NONE,
                PHYS_NONE,
                vals.len(),
                bytes.len(),
            );
            out.extend(&bytes);
        }
        COL_BOOL | COL_OPT_BOOL => {
            let mut bitmap = Vec::new();
            let mut byte: u8 = 0;
            let mut count = 0usize;
            for f in features {
                if prop_idx < f.properties.len() {
                    if let MltPropertyValue::Bool(b) = &f.properties[prop_idx] {
                        if *b {
                            byte |= 1 << (count % 8);
                        }
                        count += 1;
                        if count % 8 == 0 {
                            bitmap.push(byte);
                            byte = 0;
                        }
                    }
                }
            }
            if count % 8 != 0 {
                bitmap.push(byte);
            }
            write_stream_meta(
                out,
                STREAM_DATA,
                DATA_NONE,
                LOG_NONE,
                LOG_NONE,
                PHYS_NONE,
                count,
                bitmap.len(),
            );
            out.extend(&bitmap);
        }
        _ => {}
    }
}

fn encode_dictionary_streams(
    out: &mut Vec<u8>,
    dict_entries: &[&str],
    dict_lengths: &[u32],
    dict_data_bytes: usize,
    indices: &[u32],
) {
    // Dictionary lengths stream
    let dl_bytes = encode_varint_u32_stream(dict_lengths);
    write_stream_meta(
        out,
        STREAM_LENGTH,
        LENGTH_DICTIONARY,
        LOG_NONE,
        LOG_NONE,
        PHYS_VARINT,
        dict_lengths.len(),
        dl_bytes.len(),
    );
    out.extend(&dl_bytes);

    // Dictionary data stream
    write_stream_meta(
        out,
        STREAM_DATA,
        DATA_SINGLE,
        LOG_NONE,
        LOG_NONE,
        PHYS_NONE,
        dict_entries.len(),
        dict_data_bytes,
    );
    out.extend(
        dict_entries
            .iter()
            .flat_map(|s| s.as_bytes())
            .copied()
            .collect::<Vec<u8>>(),
    );

    // Indices stream
    let idx_bytes = encode_varint_u32_stream(indices);
    write_stream_meta(
        out,
        STREAM_OFFSET,
        OFFSET_STRING,
        LOG_NONE,
        LOG_NONE,
        PHYS_VARINT,
        indices.len(),
        idx_bytes.len(),
    );
    out.extend(&idx_bytes);
}

// ============================================================================
// Encoding Utilities
// ============================================================================

/// Zigzag encode a signed 32-bit integer.
#[inline]
fn zigzag_encode_i32(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

/// Count consecutive runs in a slice.
fn count_runs<T: PartialEq>(values: &[T]) -> usize {
    if values.is_empty() {
        return 0;
    }
    let mut runs = 1;
    for i in 1..values.len() {
        if values[i] != values[i - 1] {
            runs += 1;
        }
    }
    runs
}

/// ORC-style byte-RLE encoding.
fn byte_rle_encode(values: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if values.is_empty() {
        return out;
    }
    let n = values.len();
    let mut i = 0;
    while i < n {
        let val = values[i];
        let mut run_len = 1usize;
        while i + run_len < n && values[i + run_len] == val && run_len < 130 {
            run_len += 1;
        }
        if run_len >= 3 {
            out.push((run_len - 3) as u8);
            out.push(val);
            i += run_len;
        } else {
            let start = i;
            let mut lit_len = 0usize;
            while i + lit_len < n && lit_len < 128 {
                let v = values[i + lit_len];
                let mut ahead = 1usize;
                while i + lit_len + ahead < n && values[i + lit_len + ahead] == v && ahead < 3 {
                    ahead += 1;
                }
                if ahead >= 3 && lit_len > 0 {
                    break;
                }
                lit_len += 1;
            }
            out.push((256 - lit_len) as u8);
            out.extend_from_slice(&values[start..start + lit_len]);
            i += lit_len;
        }
    }
    out
}

/// Integer RLE: two-buffer format for MLT.
fn integer_rle_encode_u32(values: &[u32]) -> (Vec<u8>, usize, usize) {
    if values.is_empty() {
        return (Vec::new(), 0, 0);
    }
    let mut run_lengths: Vec<u32> = Vec::new();
    let mut run_values: Vec<u32> = Vec::new();
    let mut i = 0;
    while i < values.len() {
        let val = values[i];
        let mut count = 1u32;
        while i + (count as usize) < values.len() && values[i + count as usize] == val {
            count += 1;
        }
        run_lengths.push(count);
        run_values.push(val);
        i += count as usize;
    }
    let num_runs = run_lengths.len();
    let num_rle_values = values.len();
    let mut out = Vec::new();
    for &rl in &run_lengths {
        let mut buf = [0u8; 5];
        let n = rl.encode_var(&mut buf);
        out.extend_from_slice(&buf[..n]);
    }
    for &rv in &run_values {
        let mut buf = [0u8; 5];
        let n = rv.encode_var(&mut buf);
        out.extend_from_slice(&buf[..n]);
    }
    (out, num_runs, num_rle_values)
}

/// Delta encode a slice of i32 values.
fn delta_encode_i32(values: &[i32]) -> Vec<i32> {
    let mut result = Vec::with_capacity(values.len());
    let mut prev = 0i32;
    for &v in values {
        result.push(v - prev);
        prev = v;
    }
    result
}

/// Convert longitude to tile coordinate.
fn lon_to_tile_coord(lon: f64, west: f64, east: f64) -> i32 {
    ((lon - west) / (east - west) * EXTENT as f64).round() as i32
}

/// Convert latitude to tile coordinate using Mercator projection.
fn lat_to_tile_coord(lat: f64, south: f64, north: f64) -> i32 {
    let lat_merc = lat.to_radians().tan().asinh();
    let south_merc = south.to_radians().tan().asinh();
    let north_merc = north.to_radians().tan().asinh();
    ((north_merc - lat_merc) / (north_merc - south_merc) * EXTENT as f64).round() as i32
}

// ============================================================================
// Stream Writing
// ============================================================================

fn write_varint_u32(out: &mut Vec<u8>, value: u32) {
    let mut buf = [0u8; 5];
    let n = value.encode_var(&mut buf);
    out.extend_from_slice(&buf[..n]);
}

fn write_varint_usize(out: &mut Vec<u8>, value: usize) {
    let mut buf = [0u8; 10];
    let n = (value as u64).encode_var(&mut buf);
    out.extend_from_slice(&buf[..n]);
}

fn write_string(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    let mut buf = [0u8; 10];
    let n = (bytes.len() as u64).encode_var(&mut buf);
    out.extend_from_slice(&buf[..n]);
    out.extend_from_slice(bytes);
}

fn write_stream_meta(
    out: &mut Vec<u8>,
    physical_stream_type: u8,
    logical_subtype: u8,
    logical_technique1: u8,
    logical_technique2: u8,
    physical_technique: u8,
    num_values: usize,
    byte_length: usize,
) {
    let byte0 = (physical_stream_type << 4) | logical_subtype;
    let byte1 = (logical_technique1 << 5) | (logical_technique2 << 2) | physical_technique;
    out.push(byte0);
    out.push(byte1);
    write_varint_usize(out, num_values);
    write_varint_usize(out, byte_length);
}

fn write_stream_meta_rle(
    out: &mut Vec<u8>,
    physical_stream_type: u8,
    logical_subtype: u8,
    logical_technique1: u8,
    logical_technique2: u8,
    physical_technique: u8,
    num_values: usize,
    byte_length: usize,
    num_runs: usize,
    num_rle_values: usize,
) {
    let byte0 = (physical_stream_type << 4) | logical_subtype;
    let byte1 = (logical_technique1 << 5) | (logical_technique2 << 2) | physical_technique;
    out.push(byte0);
    out.push(byte1);
    write_varint_usize(out, num_values);
    write_varint_usize(out, byte_length);
    write_varint_usize(out, num_runs);
    write_varint_usize(out, num_rle_values);
}

fn encode_varint_u32_stream(values: &[u32]) -> Vec<u8> {
    let mut out = Vec::new();
    for &v in values {
        let mut buf = [0u8; 5];
        let n = v.encode_var(&mut buf);
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn encode_varint_u64_stream(values: &[u64]) -> Vec<u8> {
    let mut out = Vec::new();
    for &v in values {
        let mut buf = [0u8; 10];
        let n = v.encode_var(&mut buf);
        out.extend_from_slice(&buf[..n]);
    }
    out
}

fn encode_zigzag_varint_i64_stream(values: &[i64]) -> Vec<u8> {
    let mut out = Vec::new();
    for &v in values {
        let mut buf = [0u8; 10];
        let n = v.encode_var(&mut buf);
        out.extend_from_slice(&buf[..n]);
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{coord, point, polygon};

    // --- Byte-RLE decoder for testing ---
    fn byte_rle_decode(input: &[u8], num_bytes: usize) -> Vec<u8> {
        let mut output = Vec::with_capacity(num_bytes);
        let mut pos = 0;
        while output.len() < num_bytes && pos < input.len() {
            let control = input[pos];
            pos += 1;
            if control >= 128 {
                let count = usize::from(control ^ 0xFF) + 1;
                output.extend_from_slice(&input[pos..pos + count]);
                pos += count;
            } else {
                let count = usize::from(control) + 3;
                let value = input[pos];
                pos += 1;
                output.extend(std::iter::repeat_n(value, count));
            }
        }
        output
    }

    // --- Integer RLE decoder for testing ---
    fn integer_rle_decode_u32(encoded: &[u8], num_runs: usize, _num_rle_values: usize) -> Vec<u32> {
        let mut data: Vec<u32> = Vec::new();
        let mut offset = 0;
        while offset < encoded.len() {
            let (val, bytes_read): (u32, usize) = u32::decode_var(&encoded[offset..]).unwrap();
            data.push(val);
            offset += bytes_read;
        }
        let (run_lens, values) = data.split_at(num_runs);
        let mut result = Vec::new();
        for (&run, &val) in run_lens.iter().zip(values.iter()) {
            result.extend(std::iter::repeat_n(val, run as usize));
        }
        result
    }

    #[test]
    fn test_byte_rle_uniform_run() {
        let input = vec![5u8; 10];
        let encoded = byte_rle_encode(&input);
        let decoded = byte_rle_decode(&encoded, input.len());
        assert_eq!(decoded, input);
        assert_eq!(encoded.len(), 2);
        assert_eq!(encoded[0], 7); // 10-3 = 7
        assert_eq!(encoded[1], 5);
    }

    #[test]
    fn test_byte_rle_minimum_run() {
        let input = vec![42u8; 3];
        let encoded = byte_rle_encode(&input);
        let decoded = byte_rle_decode(&encoded, input.len());
        assert_eq!(decoded, input);
        assert_eq!(encoded[0], 0); // 3-3 = 0
        assert_eq!(encoded[1], 42);
    }

    #[test]
    fn test_byte_rle_literals() {
        let input = vec![1u8, 2];
        let encoded = byte_rle_encode(&input);
        let decoded = byte_rle_decode(&encoded, input.len());
        assert_eq!(decoded, input);
        assert_eq!(encoded[0], 254u8);
        assert_eq!(&encoded[1..], &[1, 2]);
    }

    #[test]
    fn test_byte_rle_mixed() {
        let input = vec![1, 2, 3, 3, 3, 3, 3];
        let encoded = byte_rle_encode(&input);
        let decoded = byte_rle_decode(&encoded, input.len());
        assert_eq!(decoded, input);
    }

    #[test]
    fn test_integer_rle_uniform() {
        let input = vec![42u32; 100];
        let (encoded, num_runs, num_rle_values) = integer_rle_encode_u32(&input);
        assert_eq!(num_runs, 1);
        assert_eq!(num_rle_values, 100);
        let decoded = integer_rle_decode_u32(&encoded, num_runs, num_rle_values);
        assert_eq!(decoded, input);
    }

    #[test]
    fn test_integer_rle_two_runs() {
        let mut input = vec![1u32; 50];
        input.extend(vec![2u32; 50]);
        let (encoded, num_runs, num_rle_values) = integer_rle_encode_u32(&input);
        assert_eq!(num_runs, 2);
        assert_eq!(num_rle_values, 100);
        let decoded = integer_rle_decode_u32(&encoded, num_runs, num_rle_values);
        assert_eq!(decoded, input);
    }

    #[test]
    fn test_zigzag_encode() {
        assert_eq!(zigzag_encode_i32(0), 0);
        assert_eq!(zigzag_encode_i32(-1), 1);
        assert_eq!(zigzag_encode_i32(1), 2);
        assert_eq!(zigzag_encode_i32(-2), 3);
        assert_eq!(zigzag_encode_i32(2), 4);
    }

    #[test]
    fn test_delta_encode() {
        let values = vec![10, 15, 12, 20];
        let deltas = delta_encode_i32(&values);
        assert_eq!(deltas, vec![10, 5, -3, 8]);
    }

    #[test]
    fn test_count_runs() {
        assert_eq!(count_runs::<u32>(&[]), 0);
        assert_eq!(count_runs(&[1u32]), 1);
        assert_eq!(count_runs(&[1u32, 1, 1, 1]), 1);
        assert_eq!(count_runs(&[1u32, 2, 3, 4]), 4);
        assert_eq!(count_runs(&[1u32, 1, 2, 2, 3]), 3);
    }

    #[test]
    fn test_encode_empty_tile() {
        let coord = TileCoord::new(0, 0, 0);
        let features: Vec<MltFeature> = vec![];
        let result = encode_tile_mlt(&coord, &features, "test", &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn test_encode_single_point() {
        let coord = TileCoord::new(0, 0, 0);
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
            properties: vec![],
        }];
        let result = encode_tile_mlt(&coord, &features, "test", &[]);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_polygon() {
        let coord = TileCoord::new(0, 0, 0);
        let poly = polygon![
            (x: -10.0, y: -10.0),
            (x: 10.0, y: -10.0),
            (x: 10.0, y: 10.0),
            (x: -10.0, y: 10.0),
            (x: -10.0, y: -10.0),
        ];
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::Polygon(poly),
            properties: vec![],
        }];
        let result = encode_tile_mlt(&coord, &features, "test", &[]);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_with_properties() {
        let coord = TileCoord::new(0, 0, 0);
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
            properties: vec![
                MltPropertyValue::String("test".to_string()),
                MltPropertyValue::Int(42),
                MltPropertyValue::Double(3.15), // Arbitrary test value
                MltPropertyValue::Bool(true),
            ],
        }];
        let property_names = vec![
            "name".to_string(),
            "count".to_string(),
            "value".to_string(),
            "active".to_string(),
        ];
        let result = encode_tile_mlt(&coord, &features, "test", &property_names);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_encode_multiple_features_uniform_geometry() {
        // 50 polygons - geometry type stream should use RLE
        let coord = TileCoord::new(0, 0, 0);
        let poly = polygon![
            (x: -1.0, y: -1.0),
            (x: 1.0, y: -1.0),
            (x: 1.0, y: 1.0),
            (x: -1.0, y: 1.0),
            (x: -1.0, y: -1.0),
        ];
        let features: Vec<MltFeature> = (0..50)
            .map(|i| MltFeature {
                id: Some(i as u64),
                geometry: MltGeometry::Polygon(poly.clone()),
                properties: vec![],
            })
            .collect();
        let result = encode_tile_mlt(&coord, &features, "test", &[]);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_dictionary_encoding_low_cardinality() {
        let coord = TileCoord::new(0, 0, 0);
        let categories = ["urban", "rural", "suburban"];
        let features: Vec<MltFeature> = (0..30)
            .map(|i| MltFeature {
                id: Some(i as u64),
                geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
                properties: vec![MltPropertyValue::String(categories[i % 3].to_string())],
            })
            .collect();
        let property_names = vec!["category".to_string()];

        // Verify dictionary should be used
        assert!(should_use_dictionary(&features, 0));

        let result = encode_tile_mlt(&coord, &features, "test", &property_names);
        assert!(!result.is_empty());
    }

    #[test]
    fn test_dictionary_encoding_all_unique() {
        let features: Vec<MltFeature> = (0..10)
            .map(|i| MltFeature {
                id: Some(i as u64),
                geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
                properties: vec![MltPropertyValue::String(format!("unique_{}", i))],
            })
            .collect();

        // All unique - dictionary should NOT be used
        assert!(!should_use_dictionary(&features, 0));
    }

    #[test]
    fn test_infer_column_type() {
        // All strings
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
            properties: vec![MltPropertyValue::String("test".to_string())],
        }];
        assert_eq!(infer_column_type(&features, 0), COL_STR);

        // All ints
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
            properties: vec![MltPropertyValue::Int(42)],
        }];
        assert_eq!(infer_column_type(&features, 0), COL_I64);

        // With nulls
        let features = vec![
            MltFeature {
                id: Some(1),
                geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
                properties: vec![MltPropertyValue::Int(42)],
            },
            MltFeature {
                id: Some(2),
                geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
                properties: vec![MltPropertyValue::Null],
            },
        ];
        assert_eq!(infer_column_type(&features, 0), COL_OPT_I64);
    }

    #[test]
    fn test_coordinate_transforms() {
        // Test longitude to tile coordinate
        let x = lon_to_tile_coord(0.0, -180.0, 180.0);
        assert_eq!(x, (EXTENT / 2) as i32);

        // Test latitude to tile coordinate (Mercator)
        let y = lat_to_tile_coord(0.0, -85.0, 85.0);
        assert!((y - (EXTENT / 2) as i32).abs() < 10); // Approximately center
    }

    #[test]
    fn test_geometry_type_byte() {
        assert_eq!(
            geometry_type_byte(&MltGeometry::Point(point!(x: 0.0, y: 0.0))),
            GEOM_POINT
        );
        assert_eq!(
            geometry_type_byte(&MltGeometry::MultiPoint(MultiPoint(vec![
                point!(x: 0.0, y: 0.0)
            ]))),
            GEOM_MULTI_POINT
        );
        assert_eq!(
            geometry_type_byte(&MltGeometry::LineString(LineString(vec![
                coord! {x: 0.0, y: 0.0},
                coord! {x: 1.0, y: 1.0}
            ]))),
            GEOM_LINESTRING
        );
        assert_eq!(
            geometry_type_byte(&MltGeometry::Polygon(polygon![
                (x: 0.0, y: 0.0),
                (x: 1.0, y: 0.0),
                (x: 1.0, y: 1.0),
                (x: 0.0, y: 0.0),
            ])),
            GEOM_POLYGON
        );
    }

    #[test]
    fn test_count_geometry_streams() {
        // Points only - 2 streams (type + vertex)
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::Point(point!(x: 0.0, y: 0.0)),
            properties: vec![],
        }];
        assert_eq!(count_geometry_streams(&features), 2);

        // Polygons - 4 streams (type + vertex + parts + rings)
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::Polygon(polygon![
                (x: 0.0, y: 0.0),
                (x: 1.0, y: 0.0),
                (x: 1.0, y: 1.0),
                (x: 0.0, y: 0.0),
            ]),
            properties: vec![],
        }];
        assert_eq!(count_geometry_streams(&features), 4);

        // MultiPolygons - 5 streams (type + vertex + geometries + parts + rings)
        let features = vec![MltFeature {
            id: Some(1),
            geometry: MltGeometry::MultiPolygon(MultiPolygon(vec![polygon![
                (x: 0.0, y: 0.0),
                (x: 1.0, y: 0.0),
                (x: 1.0, y: 1.0),
                (x: 0.0, y: 0.0),
            ]])),
            properties: vec![],
        }];
        assert_eq!(count_geometry_streams(&features), 5);
    }
}
