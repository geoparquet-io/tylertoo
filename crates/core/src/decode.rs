//! PMTiles → GeoParquet decoding (issue #112).
//!
//! Decodes a PMTiles v3 vector-tile archive back into a GeoParquet file,
//! following the model of `tippecanoe-decode` (decode.cpp / write_json.cpp):
//!
//! - **Every feature from every selected tile is emitted — no deduplication.**
//!   A feature near a tile seam appears once per neighboring tile (buffer
//!   copies), and once per zoom level of its band. This matches
//!   tippecanoe-decode; filter by the `zoom` column (or `--zoom` in the CLI)
//!   for a single representation.
//! - The output is the **tiled representation**, not the original source:
//!   geometries are simplified per zoom, clipped to (buffered) tile bounds,
//!   and properties are whatever survived tiling. There is no round-trip
//!   guarantee. Extract the maximum zoom for the best available detail.
//!
//! # Coordinate transform
//!
//! Tile-local integer coordinates are lifted to WGS84 through tippecanoe's
//! 32-bit Web Mercator world coordinate space (write_json.cpp /
//! projection.cpp):
//!
//! ```text
//! wscale = 2^(32 - z)
//! wx = wscale * tile_x + (wscale / extent) * px
//! lon = wx / 2^32 * 360 - 180
//! lat = atan(sinh(pi - 2*pi * wy / 2^32)) in degrees
//! ```
//!
//! Computed in f64, which reproduces tippecanoe's integer arithmetic exactly
//! for power-of-two extents up to `z + log2(extent) <= 52` (i.e. everywhere
//! in practice; MVT extents are powers of two and z <= 31).
//!
//! # Output schema
//!
//! Three provenance columns are always present so users can filter the
//! duplicated representation: `zoom` (UInt8), `layer` (Utf8), and `mvt_id`
//! (UInt64, nullable — the raw MVT feature id, if any). They are followed by
//! the union of all property columns seen across tiles/layers (alphabetical,
//! all nullable; a feature lacking a property gets null), then `geometry`.
//! A property named like a provenance column is rejected with
//! [`DecodeError::ReservedColumn`].
//!
//! Property types are unified across features: `int`/`sint`/`uint` → Int64
//! (a `uint` above `i64::MAX` degrades to Float64, like JSON consumers),
//! `float`/`double` → Float64, Int64 ∪ Float64 → Float64, and any other
//! mixture (e.g. bool vs string) degrades to Utf8 with values stringified.
//!
//! # Divergences from tippecanoe
//!
//! - DIVERGENCE FROM TIPPECANOE: tippecanoe-decode emits GeoJSON text; we
//!   emit GeoParquet through the existing GeoArrow writer stack, which is
//!   the point of the feature.
//! - Degenerate MVT content (zero-area rings, one-point linestrings,
//!   interior rings before any exterior ring) is dropped rather than
//!   emitted; the MVT spec leaves decoder behavior for these undefined.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arrow_array::builder::{
    BooleanBuilder, Float64Builder, Int64Builder, StringBuilder, UInt64Builder, UInt8Builder,
};
use arrow_array::{ArrayRef, RecordBatch};
use arrow_schema::{DataType, Field, Schema};
use geo::{Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon};
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use parquet::arrow::ArrowWriter;
use prost::Message;
use serde::Serialize;
use thiserror::Error;

use crate::compression::decompress;
use crate::mvt::{command_decode, zigzag_decode};
use crate::pmtiles_writer::{decode_directory, tile_id_to_zxy, Header, TileType};
use crate::vector_tile::tile::GeomType;
use crate::vector_tile::Tile;

/// Rows per output batch (bounds decoder memory to O(batch), matching the
/// export pipeline's streaming ethos).
const BATCH_SIZE: usize = 8192;

/// Provenance / structural column names reserved by the decoder.
const RESERVED_COLUMNS: [&str; 4] = ["zoom", "layer", "mvt_id", "geometry"];

// ============================================================================
// Public API
// ============================================================================

/// Options for [`decode_pmtiles`].
#[derive(Debug, Clone, Default)]
pub struct DecodeOptions {
    /// Only decode tiles with `zoom >= min_zoom`.
    pub min_zoom: Option<u8>,
    /// Only decode tiles with `zoom <= max_zoom`.
    pub max_zoom: Option<u8>,
    /// Only decode features from this MVT layer.
    pub layer: Option<String>,
}

/// Summary of a completed decode.
#[derive(Debug, Clone, Serialize)]
pub struct DecodeReport {
    /// Tiles read (after zoom filtering).
    pub tiles_read: u64,
    /// Feature rows written to the output.
    pub features_written: u64,
    /// Features skipped because their geometry decoded to nothing
    /// (empty / fully degenerate per the module docs).
    pub features_skipped: u64,
    /// MVT layers encountered (after layer filtering), sorted.
    pub layers: Vec<String>,
    /// Min/max zoom actually written (None when no features were written).
    pub zoom_range: Option<(u8, u8)>,
    /// Wall-clock seconds.
    pub elapsed_secs: f64,
}

/// Errors from [`decode_pmtiles`].
#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Archive(#[from] crate::Error),

    #[error("invalid PMTiles archive: {0}")]
    InvalidArchive(String),

    #[error("archive does not contain vector tiles (tile type {0:?})")]
    NotVectorTiles(TileType),

    #[error("MVT protobuf decode failed for tile z{z}/{x}/{y}: {source}")]
    Mvt {
        z: u8,
        x: u32,
        y: u32,
        source: prost::DecodeError,
    },

    #[error("invalid MVT geometry in tile z{z}/{x}/{y}: {reason}")]
    Geometry {
        z: u8,
        x: u32,
        y: u32,
        reason: String,
    },

    #[error(
        "property column {0:?} collides with a decoder provenance column; \
         rename it in the source data or decode with a layer filter"
    )]
    ReservedColumn(String),

    #[error("Arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),

    #[error("Parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),

    #[error("GeoParquet write failed: {0}")]
    Write(String),
}

/// Decode a PMTiles archive into a GeoParquet file.
///
/// See the module docs for output semantics (no deduplication, tiled
/// representation, provenance columns, property type unification).
pub fn decode_pmtiles(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &DecodeOptions,
) -> Result<DecodeReport, DecodeError> {
    let start = Instant::now();
    let bytes = std::fs::read(input_path.as_ref())?;
    let header = Header::from_bytes(&bytes)?;
    if header.tile_type != TileType::Mvt {
        return Err(DecodeError::NotVectorTiles(header.tile_type));
    }

    let tiles = collect_tile_refs(&bytes, &header, options)?;

    // ---- Pass A: property schema union + layer inventory. -----------------
    // Tiles are decoded twice (schema first, rows second) so peak memory
    // stays O(one tile + one batch) instead of O(all features).
    let mut col_types: BTreeMap<String, ColType> = BTreeMap::new();
    let mut layers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for tile in &tiles {
        for (layer_name, feature) in decode_tile_features(&bytes, &header, tile, options)? {
            layers.insert(layer_name);
            for (key, value) in &feature.props {
                if RESERVED_COLUMNS.contains(&key.as_str()) {
                    return Err(DecodeError::ReservedColumn(key.clone()));
                }
                let t = ColType::of(value);
                col_types
                    .entry(key.clone())
                    .and_modify(|e| *e = e.unify(t))
                    .or_insert(t);
            }
        }
    }

    // ---- Output schema: zoom, layer, mvt_id, properties (sorted), geometry.
    let geom_field = GeometryBuilder::new(GeometryType::new(Default::default()))
        .finish()
        .data_type()
        .to_field("geometry", true);
    let mut fields: Vec<Arc<Field>> = vec![
        Arc::new(Field::new("zoom", DataType::UInt8, false)),
        Arc::new(Field::new("layer", DataType::Utf8, false)),
        Arc::new(Field::new("mvt_id", DataType::UInt64, true)),
    ];
    for (name, t) in &col_types {
        fields.push(Arc::new(Field::new(name, t.arrow_type(), true)));
    }
    fields.push(Arc::new(geom_field));
    let schema = Arc::new(Schema::new(fields));

    let gpq_options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options)
        .map_err(|e| DecodeError::Write(e.to_string()))?;
    let file = std::fs::File::create(output_path.as_ref())?;
    let mut writer = ArrowWriter::try_new(file, encoder.target_schema(), None)?;

    // ---- Pass B: decode again, batch rows, stream to the writer. ----------
    let mut report = DecodeReport {
        tiles_read: tiles.len() as u64,
        features_written: 0,
        features_skipped: 0,
        layers: layers.into_iter().collect(),
        zoom_range: None,
        elapsed_secs: 0.0,
    };
    let mut batch = RowBatch::new(&col_types);
    for tile in &tiles {
        for (layer_name, feature) in decode_tile_features(&bytes, &header, tile, options)? {
            match feature.geometry {
                Some(geometry) => {
                    batch.push(
                        tile.z,
                        &layer_name,
                        feature.mvt_id,
                        &feature.props,
                        geometry,
                    );
                    report.features_written += 1;
                    report.zoom_range = Some(match report.zoom_range {
                        Some((lo, hi)) => (lo.min(tile.z), hi.max(tile.z)),
                        None => (tile.z, tile.z),
                    });
                    if batch.len() >= BATCH_SIZE {
                        flush_batch(&mut batch, &schema, &mut encoder, &mut writer, &col_types)?;
                    }
                }
                None => report.features_skipped += 1,
            }
        }
    }
    if batch.len() > 0 {
        flush_batch(&mut batch, &schema, &mut encoder, &mut writer, &col_types)?;
    }

    writer.append_key_value_metadata(
        encoder
            .into_keyvalue()
            .map_err(|e| DecodeError::Write(e.to_string()))?,
    );
    writer.close()?;

    report.elapsed_secs = start.elapsed().as_secs_f64();
    Ok(report)
}

// ============================================================================
// Archive walking
// ============================================================================

/// A located tile: coordinates plus the byte range of its (compressed) data.
struct TileRef {
    z: u8,
    x: u32,
    y: u32,
    start: usize,
    len: usize,
}

/// Walk the root (and any leaf) directories, expand run-length entries, and
/// apply the zoom filter. Entries arrive in ascending tile-id order (the
/// writer emits clustered archives), which the output preserves.
fn collect_tile_refs(
    bytes: &[u8],
    header: &Header,
    options: &DecodeOptions,
) -> Result<Vec<TileRef>, DecodeError> {
    let section = |offset: u64, length: u64, what: &str| -> Result<&[u8], DecodeError> {
        let start = usize::try_from(offset)
            .map_err(|_| DecodeError::InvalidArchive(format!("{what} offset overflow")))?;
        let end = start
            .checked_add(
                usize::try_from(length)
                    .map_err(|_| DecodeError::InvalidArchive(format!("{what} length overflow")))?,
            )
            .filter(|&e| e <= bytes.len())
            .ok_or_else(|| {
                DecodeError::InvalidArchive(format!("{what} extends past end of file"))
            })?;
        Ok(&bytes[start..end])
    };

    let decode_dir = |raw: &[u8], what: &str| -> Result<Vec<_>, DecodeError> {
        let plain = decompress(raw, header.internal_compression)?;
        decode_directory(&plain)
            .ok_or_else(|| DecodeError::InvalidArchive(format!("undecodable {what}")))
    };

    let root_raw = section(
        header.root_dir_offset,
        header.root_dir_length,
        "root directory",
    )?;
    let root = decode_dir(root_raw, "root directory")?;

    let mut entries = Vec::new();
    for entry in root {
        if entry.run_length == 0 {
            // Leaf directory: offset is relative to the leaf-dirs section.
            let leaf_raw = section(
                header.leaf_dirs_offset + entry.offset,
                u64::from(entry.length),
                "leaf directory",
            )?;
            entries.extend(decode_dir(leaf_raw, "leaf directory")?);
        } else {
            entries.push(entry);
        }
    }

    let mut tiles = Vec::new();
    for entry in &entries {
        for i in 0..u64::from(entry.run_length.max(1)) {
            let (z, x, y) = tile_id_to_zxy(entry.tile_id + i)?;
            if options.min_zoom.is_some_and(|mz| z < mz)
                || options.max_zoom.is_some_and(|mz| z > mz)
            {
                continue;
            }
            // Validate the range now so pass B can slice without checks.
            section(
                header.tile_data_offset + entry.offset,
                u64::from(entry.length),
                "tile data",
            )?;
            tiles.push(TileRef {
                z,
                x,
                y,
                start: (header.tile_data_offset + entry.offset) as usize,
                len: entry.length as usize,
            });
        }
    }
    Ok(tiles)
}

// ============================================================================
// Tile decoding: MVT bytes -> features
// ============================================================================

/// One decoded feature. `geometry` is `None` when the MVT geometry decoded
/// to nothing (empty command stream / fully degenerate content).
struct DecodedFeature {
    geometry: Option<Geometry<f64>>,
    mvt_id: Option<u64>,
    props: Vec<(String, PropValue)>,
}

/// Decompress and decode one tile into `(layer_name, feature)` pairs,
/// applying the layer filter and the coordinate transform.
fn decode_tile_features(
    bytes: &[u8],
    header: &Header,
    tile: &TileRef,
    options: &DecodeOptions,
) -> Result<Vec<(String, DecodedFeature)>, DecodeError> {
    let raw = &bytes[tile.start..tile.start + tile.len];
    let plain = decompress(raw, header.tile_compression)?;
    let decoded = Tile::decode(plain.as_slice()).map_err(|source| DecodeError::Mvt {
        z: tile.z,
        x: tile.x,
        y: tile.y,
        source,
    })?;

    let mut out = Vec::new();
    for layer in &decoded.layers {
        if options.layer.as_ref().is_some_and(|l| *l != layer.name) {
            continue;
        }
        let extent = layer.extent.unwrap_or(4096);
        for feature in &layer.features {
            let parts = parse_command_stream(&feature.geometry).map_err(|reason| {
                DecodeError::Geometry {
                    z: tile.z,
                    x: tile.x,
                    y: tile.y,
                    reason,
                }
            })?;
            let geometry = assemble_geometry(feature.r#type(), &parts, |px, py| {
                tile_local_to_lonlat(tile.z, tile.x, tile.y, extent, px, py)
            });

            let mut props = Vec::new();
            for pair in feature.tags.chunks(2) {
                if pair.len() != 2 {
                    return Err(DecodeError::Geometry {
                        z: tile.z,
                        x: tile.x,
                        y: tile.y,
                        reason: "odd-length tag list".to_string(),
                    });
                }
                let key =
                    layer
                        .keys
                        .get(pair[0] as usize)
                        .ok_or_else(|| DecodeError::Geometry {
                            z: tile.z,
                            x: tile.x,
                            y: tile.y,
                            reason: format!("tag key index {} out of range", pair[0]),
                        })?;
                let value =
                    layer
                        .values
                        .get(pair[1] as usize)
                        .ok_or_else(|| DecodeError::Geometry {
                            z: tile.z,
                            x: tile.x,
                            y: tile.y,
                            reason: format!("tag value index {} out of range", pair[1]),
                        })?;
                if let Some(v) = PropValue::from_mvt(value) {
                    props.push((key.clone(), v));
                }
            }

            out.push((
                layer.name.clone(),
                DecodedFeature {
                    geometry,
                    mvt_id: feature.id,
                    props,
                },
            ));
        }
    }
    Ok(out)
}

// ============================================================================
// MVT geometry command stream -> parts -> geo::Geometry
// ============================================================================

/// One MoveTo-initiated run of vertices in tile-local coordinates.
struct Part {
    /// Vertices, without the implicit ClosePath closing vertex.
    pts: Vec<(i64, i64)>,
    /// Whether the part was terminated by ClosePath (i.e. it is a ring).
    closed: bool,
}

impl Part {
    /// Twice the signed area by the surveyor's formula on raw tile coords.
    /// Per the MVT spec, positive ⇒ exterior ring, negative ⇒ interior ring
    /// (Y axis points down in tile space).
    fn area2(&self) -> i64 {
        let n = self.pts.len();
        let mut sum = 0i64;
        for i in 0..n {
            let (x0, y0) = self.pts[i];
            let (x1, y1) = self.pts[(i + 1) % n];
            sum += x0 * y1 - x1 * y0;
        }
        sum
    }
}

/// Parse an MVT geometry command stream (MoveTo/LineTo/ClosePath with
/// zigzag-encoded deltas) into parts. Each MoveTo coordinate starts a new
/// part, so multipoints come out as one single-vertex part per point.
fn parse_command_stream(geom: &[u32]) -> Result<Vec<Part>, String> {
    const CMD_MOVE_TO: u32 = 1;
    const CMD_LINE_TO: u32 = 2;
    const CMD_CLOSE_PATH: u32 = 7;

    let mut parts: Vec<Part> = Vec::new();
    let mut cur: Vec<(i64, i64)> = Vec::new();
    let (mut cx, mut cy) = (0i64, 0i64);
    let mut i = 0usize;

    let take_pair = |i: &mut usize, cx: &mut i64, cy: &mut i64| -> Result<(), String> {
        if *i + 1 >= geom.len() {
            return Err("truncated coordinate pair".to_string());
        }
        *cx += i64::from(zigzag_decode(geom[*i]));
        *cy += i64::from(zigzag_decode(geom[*i + 1]));
        *i += 2;
        Ok(())
    };

    while i < geom.len() {
        let (cmd, count) = command_decode(geom[i]);
        i += 1;
        match cmd {
            CMD_MOVE_TO => {
                for _ in 0..count {
                    take_pair(&mut i, &mut cx, &mut cy)?;
                    if !cur.is_empty() {
                        parts.push(Part {
                            pts: std::mem::take(&mut cur),
                            closed: false,
                        });
                    }
                    cur.push((cx, cy));
                }
            }
            CMD_LINE_TO => {
                for _ in 0..count {
                    take_pair(&mut i, &mut cx, &mut cy)?;
                    cur.push((cx, cy));
                }
            }
            CMD_CLOSE_PATH => {
                if !cur.is_empty() {
                    parts.push(Part {
                        pts: std::mem::take(&mut cur),
                        closed: true,
                    });
                }
            }
            other => return Err(format!("unknown MVT command id {other}")),
        }
    }
    if !cur.is_empty() {
        parts.push(Part {
            pts: cur,
            closed: false,
        });
    }
    Ok(parts)
}

/// Assemble parsed parts into a `geo::Geometry` in WGS84, per the MVT spec
/// rules for each geometry type. Returns `None` when nothing valid remains
/// (see module docs for the degenerate cases that are dropped).
fn assemble_geometry(
    geom_type: GeomType,
    parts: &[Part],
    tf: impl Fn(i64, i64) -> (f64, f64),
) -> Option<Geometry<f64>> {
    match geom_type {
        GeomType::Point => {
            let pts: Vec<Point<f64>> = parts
                .iter()
                .flat_map(|p| p.pts.iter())
                .map(|&(x, y)| Point::from(tf(x, y)))
                .collect();
            match pts.len() {
                0 => None,
                1 => Some(Geometry::Point(pts.into_iter().next().expect("len 1"))),
                _ => Some(Geometry::MultiPoint(MultiPoint::new(pts))),
            }
        }
        GeomType::Linestring => {
            let lines: Vec<LineString<f64>> = parts
                .iter()
                .filter(|p| p.pts.len() >= 2)
                .map(|p| LineString::from(p.pts.iter().map(|&(x, y)| tf(x, y)).collect::<Vec<_>>()))
                .collect();
            match lines.len() {
                0 => None,
                1 => Some(Geometry::LineString(
                    lines.into_iter().next().expect("len 1"),
                )),
                _ => Some(Geometry::MultiLineString(MultiLineString::new(lines))),
            }
        }
        GeomType::Polygon => {
            let mut polys: Vec<(LineString<f64>, Vec<LineString<f64>>)> = Vec::new();
            for part in parts {
                // Rings must be closed and have >= 3 vertices; zero-area
                // rings carry no winding information. Drop degenerates.
                if !part.closed || part.pts.len() < 3 {
                    continue;
                }
                let area2 = part.area2();
                if area2 == 0 {
                    continue;
                }
                let ring =
                    LineString::from(part.pts.iter().map(|&(x, y)| tf(x, y)).collect::<Vec<_>>());
                if area2 > 0 {
                    polys.push((ring, Vec::new()));
                } else if let Some(last) = polys.last_mut() {
                    last.1.push(ring);
                }
                // Interior ring before any exterior: dropped (undefined per
                // MVT spec; see module docs).
            }
            let polys: Vec<Polygon<f64>> = polys
                .into_iter()
                .map(|(ext, holes)| Polygon::new(ext, holes))
                .collect();
            match polys.len() {
                0 => None,
                1 => Some(Geometry::Polygon(polys.into_iter().next().expect("len 1"))),
                _ => Some(Geometry::MultiPolygon(MultiPolygon::new(polys))),
            }
        }
        GeomType::Unknown => None,
    }
}

// ============================================================================
// Coordinate transform (tippecanoe write_json.cpp / projection.cpp)
// ============================================================================

/// Transform tile-local coordinates to WGS84 lon/lat.
///
/// See the module docs for the formula and its exactness envelope. Buffered
/// coordinates (px/py outside `0..extent`) transform fine — they simply land
/// outside the tile's geographic bounds, exactly as tippecanoe-decode emits
/// them.
pub fn tile_local_to_lonlat(z: u8, tx: u32, ty: u32, extent: u32, px: i64, py: i64) -> (f64, f64) {
    const WORLD: f64 = 4_294_967_296.0; // 2^32
    let wscale = 2f64.powi(32 - i32::from(z));
    let wx = wscale * f64::from(tx) + (wscale / f64::from(extent)) * px as f64;
    let wy = wscale * f64::from(ty) + (wscale / f64::from(extent)) * py as f64;
    let lon = wx / WORLD * 360.0 - 180.0;
    let n = std::f64::consts::PI - 2.0 * std::f64::consts::PI * wy / WORLD;
    let lat = n.sinh().atan().to_degrees();
    (lon, lat)
}

// ============================================================================
// Properties: MVT Value -> unified Arrow columns
// ============================================================================

/// A decoded MVT property value (already normalized: uint that fits i64 is
/// Int, larger uints degrade to Float — see module docs).
#[derive(Debug, Clone, PartialEq)]
enum PropValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

impl PropValue {
    /// Decode an MVT `Value`. Returns `None` for a value with no field set
    /// (invalid per spec; skipped rather than fatal).
    fn from_mvt(v: &crate::vector_tile::tile::Value) -> Option<Self> {
        if let Some(s) = &v.string_value {
            Some(PropValue::Str(s.clone()))
        } else if let Some(f) = v.float_value {
            Some(PropValue::Float(f64::from(f)))
        } else if let Some(d) = v.double_value {
            Some(PropValue::Float(d))
        } else if let Some(i) = v.int_value {
            Some(PropValue::Int(i))
        } else if let Some(u) = v.uint_value {
            match i64::try_from(u) {
                Ok(i) => Some(PropValue::Int(i)),
                Err(_) => Some(PropValue::Float(u as f64)),
            }
        } else if let Some(i) = v.sint_value {
            Some(PropValue::Int(i))
        } else {
            v.bool_value.map(PropValue::Bool)
        }
    }

    /// Stringified form, used when a column degrades to Utf8.
    fn to_string_lossy(&self) -> String {
        match self {
            PropValue::Bool(b) => b.to_string(),
            PropValue::Int(i) => i.to_string(),
            PropValue::Float(f) => f.to_string(),
            PropValue::Str(s) => s.clone(),
        }
    }
}

/// Unified Arrow column type for a property across all features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColType {
    Bool,
    Int,
    Float,
    Str,
}

impl ColType {
    fn of(v: &PropValue) -> Self {
        match v {
            PropValue::Bool(_) => ColType::Bool,
            PropValue::Int(_) => ColType::Int,
            PropValue::Float(_) => ColType::Float,
            PropValue::Str(_) => ColType::Str,
        }
    }

    /// Promotion lattice: identical stays; Int ∪ Float = Float; everything
    /// else (bool vs number, anything vs string) degrades to Utf8.
    fn unify(self, other: Self) -> Self {
        use ColType::*;
        match (self, other) {
            (a, b) if a == b => a,
            (Int, Float) | (Float, Int) => Float,
            _ => Str,
        }
    }

    fn arrow_type(self) -> DataType {
        match self {
            ColType::Bool => DataType::Boolean,
            ColType::Int => DataType::Int64,
            ColType::Float => DataType::Float64,
            ColType::Str => DataType::Utf8,
        }
    }
}

// ============================================================================
// Row batching -> Arrow -> GeoParquet
// ============================================================================

/// Column-oriented buffer for one output batch.
struct RowBatch {
    zoom: UInt8Builder,
    layer: StringBuilder,
    mvt_id: UInt64Builder,
    props: BTreeMap<String, PropColumn>,
    geoms: Vec<Geometry<f64>>,
}

enum PropColumn {
    Bool(BooleanBuilder),
    Int(Int64Builder),
    Float(Float64Builder),
    Str(StringBuilder),
}

impl PropColumn {
    fn for_type(t: ColType) -> Self {
        match t {
            ColType::Bool => PropColumn::Bool(BooleanBuilder::new()),
            ColType::Int => PropColumn::Int(Int64Builder::new()),
            ColType::Float => PropColumn::Float(Float64Builder::new()),
            ColType::Str => PropColumn::Str(StringBuilder::new()),
        }
    }

    /// Append a value, coercing per the promotion lattice (a column typed
    /// Float may receive Int values; a column typed Str may receive any).
    fn append(&mut self, v: Option<&PropValue>) {
        match (self, v) {
            (PropColumn::Bool(b), Some(PropValue::Bool(x))) => b.append_value(*x),
            (PropColumn::Int(b), Some(PropValue::Int(x))) => b.append_value(*x),
            (PropColumn::Float(b), Some(PropValue::Float(x))) => b.append_value(*x),
            (PropColumn::Float(b), Some(PropValue::Int(x))) => b.append_value(*x as f64),
            (PropColumn::Str(b), Some(x)) => b.append_value(x.to_string_lossy()),
            (PropColumn::Bool(b), None) => b.append_null(),
            (PropColumn::Int(b), None) => b.append_null(),
            (PropColumn::Float(b), None) => b.append_null(),
            (PropColumn::Str(b), None) => b.append_null(),
            // Unreachable by construction: pass A fixed the column type as
            // the unified supertype of every value.
            (col, Some(other)) => {
                unreachable!(
                    "value {other:?} does not fit unified column {:?}",
                    col.kind()
                )
            }
        }
    }

    fn kind(&self) -> ColType {
        match self {
            PropColumn::Bool(_) => ColType::Bool,
            PropColumn::Int(_) => ColType::Int,
            PropColumn::Float(_) => ColType::Float,
            PropColumn::Str(_) => ColType::Str,
        }
    }

    fn finish(&mut self) -> ArrayRef {
        match self {
            PropColumn::Bool(b) => Arc::new(b.finish()),
            PropColumn::Int(b) => Arc::new(b.finish()),
            PropColumn::Float(b) => Arc::new(b.finish()),
            PropColumn::Str(b) => Arc::new(b.finish()),
        }
    }
}

impl RowBatch {
    fn new(col_types: &BTreeMap<String, ColType>) -> Self {
        RowBatch {
            zoom: UInt8Builder::new(),
            layer: StringBuilder::new(),
            mvt_id: UInt64Builder::new(),
            props: col_types
                .iter()
                .map(|(k, t)| (k.clone(), PropColumn::for_type(*t)))
                .collect(),
            geoms: Vec::new(),
        }
    }

    fn len(&self) -> usize {
        self.geoms.len()
    }

    fn push(
        &mut self,
        zoom: u8,
        layer: &str,
        mvt_id: Option<u64>,
        props: &[(String, PropValue)],
        geometry: Geometry<f64>,
    ) {
        self.zoom.append_value(zoom);
        self.layer.append_value(layer);
        match mvt_id {
            Some(id) => self.mvt_id.append_value(id),
            None => self.mvt_id.append_null(),
        }
        for (name, col) in self.props.iter_mut() {
            // Last occurrence wins if a tag key repeats within one feature.
            let v = props.iter().rev().find(|(k, _)| k == name).map(|(_, v)| v);
            col.append(v);
        }
        self.geoms.push(geometry);
    }
}

/// Convert the buffered rows into a RecordBatch, encode, and write it.
fn flush_batch(
    batch: &mut RowBatch,
    schema: &Arc<Schema>,
    encoder: &mut GeoParquetRecordBatchEncoder,
    writer: &mut ArrowWriter<std::fs::File>,
    col_types: &BTreeMap<String, ColType>,
) -> Result<(), DecodeError> {
    let mut geom_builder = GeometryBuilder::new(GeometryType::new(Default::default()));
    geom_builder.extend_from_iter(batch.geoms.iter().map(Some));
    let geom_arr = geom_builder.finish();

    let mut columns: Vec<ArrayRef> = vec![
        Arc::new(batch.zoom.finish()),
        Arc::new(batch.layer.finish()),
        Arc::new(batch.mvt_id.finish()),
    ];
    for col in batch.props.values_mut() {
        columns.push(col.finish());
    }
    columns.push(geom_arr.to_array_ref());

    let record = RecordBatch::try_new(schema.clone(), columns)?;
    let encoded = encoder
        .encode_record_batch(&record)
        .map_err(|e| DecodeError::Write(e.to_string()))?;
    writer.write(&encoded)?;

    *batch = RowBatch::new(col_types);
    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{command_encode, encode_geometry, zigzag_encode};
    use crate::tile::{tile_bounds, TileBounds};
    use geo::{MultiLineString, MultiPoint, MultiPolygon};

    // ---- Coordinate transform ---------------------------------------------

    /// One tile-local unit in degrees of longitude at (z, extent).
    fn lon_unit(z: u8, extent: u32) -> f64 {
        360.0 / ((1u64 << z) as f64 * f64::from(extent))
    }

    #[test]
    fn transform_tile_origin_of_world_tile_is_northwest_corner() {
        let (lon, lat) = tile_local_to_lonlat(0, 0, 0, 4096, 0, 0);
        assert!((lon - (-180.0)).abs() < 1e-9);
        assert!((lat - 85.051_128_779_806_59).abs() < 1e-6); // top of Mercator
    }

    #[test]
    fn transform_tile_center_of_world_tile_is_null_island() {
        let (lon, lat) = tile_local_to_lonlat(0, 0, 0, 4096, 2048, 2048);
        assert!(lon.abs() < 1e-9, "{lon}");
        assert!(lat.abs() < 1e-9, "{lat}");
    }

    #[test]
    fn transform_matches_tile_bounds_corners() {
        // The tile's (0,0) and (extent,extent) corners must land on the
        // geographic bounds our own tiler computes for that tile.
        for (z, x, y) in [(1u8, 1u32, 0u32), (5, 17, 11), (14, 4823, 6160)] {
            let b = tile_bounds(x, y, z);
            let (lon0, lat0) = tile_local_to_lonlat(z, x, y, 4096, 0, 0);
            let (lon1, lat1) = tile_local_to_lonlat(z, x, y, 4096, 4096, 4096);
            assert!((lon0 - b.lng_min).abs() < 1e-9, "z{z} lng_min");
            assert!((lat0 - b.lat_max).abs() < 1e-6, "z{z} lat_max");
            assert!((lon1 - b.lng_max).abs() < 1e-9, "z{z} lng_max");
            assert!((lat1 - b.lat_min).abs() < 1e-6, "z{z} lat_min");
        }
    }

    #[test]
    fn transform_handles_buffered_coordinates_outside_tile() {
        // Buffer pixels land monotonically outside the tile bounds.
        let b = tile_bounds(100, 200, 9);
        let (lon, lat) = tile_local_to_lonlat(9, 100, 200, 4096, -64, -64);
        assert!(lon < b.lng_min);
        assert!(lat > b.lat_max);
    }

    // ---- Command stream parsing -------------------------------------------

    #[test]
    fn parse_rejects_truncated_stream() {
        // MoveTo count=1 but only one parameter integer.
        let geom = vec![command_encode(1, 1), zigzag_encode(5)];
        assert!(parse_command_stream(&geom).is_err());
    }

    #[test]
    fn parse_rejects_unknown_command() {
        let geom = vec![command_encode(5, 1), 0, 0];
        assert!(parse_command_stream(&geom).is_err());
    }

    #[test]
    fn parse_empty_stream_is_empty() {
        assert!(parse_command_stream(&[]).unwrap().is_empty());
    }

    // ---- Geometry assembly: round-trip against our own MVT encoder --------

    /// Encode a geometry with the production encoder for the given tile,
    /// then parse + assemble it back and return the decoded geometry.
    fn roundtrip(geom: &Geometry<f64>, z: u8, x: u32, y: u32) -> Option<Geometry<f64>> {
        let bounds: TileBounds = tile_bounds(x, y, z);
        // encode_geometry returns the MVT GeomType alongside the commands;
        // feed both straight into the decoder, mirroring decode_tile_features.
        let (commands, geom_type) = encode_geometry(geom, &bounds, 4096);
        let parts = parse_command_stream(&commands).unwrap();
        assemble_geometry(geom_type, &parts, |px, py| {
            tile_local_to_lonlat(z, x, y, 4096, px, py)
        })
    }

    /// Assert two coordinates match within n tile-local quantization units.
    fn assert_close(got: (f64, f64), want: (f64, f64), z: u8, what: &str) {
        let tol = lon_unit(z, 4096);
        assert!(
            (got.0 - want.0).abs() <= tol && (got.1 - want.1).abs() <= tol,
            "{what}: got {got:?}, want {want:?}, tol {tol}"
        );
    }

    #[test]
    fn assemble_point_roundtrip() {
        let z = 10;
        let b = tile_bounds(301, 385, z); // contains (-74.xx, 40.xx)
        let p = Point::new((b.lng_min + b.lng_max) / 2.0, (b.lat_min + b.lat_max) / 2.0);
        let got = roundtrip(&Geometry::Point(p), z, 301, 385).unwrap();
        match got {
            Geometry::Point(q) => assert_close((q.x(), q.y()), (p.x(), p.y()), z, "point"),
            other => panic!("expected Point, got {other:?}"),
        }
    }

    #[test]
    fn assemble_multipoint_roundtrip() {
        let z = 10;
        let (x, y) = (301, 385);
        let b = tile_bounds(x, y, z);
        let w = b.lng_max - b.lng_min;
        let h = b.lat_max - b.lat_min;
        let pts = vec![
            Point::new(b.lng_min + 0.25 * w, b.lat_min + 0.25 * h),
            Point::new(b.lng_min + 0.50 * w, b.lat_min + 0.50 * h),
            Point::new(b.lng_min + 0.75 * w, b.lat_min + 0.75 * h),
        ];
        let got = roundtrip(&Geometry::MultiPoint(MultiPoint::new(pts.clone())), z, x, y).unwrap();
        match got {
            Geometry::MultiPoint(mp) => {
                assert_eq!(mp.0.len(), 3);
                for (q, p) in mp.0.iter().zip(&pts) {
                    assert_close((q.x(), q.y()), (p.x(), p.y()), z, "multipoint vertex");
                }
            }
            other => panic!("expected MultiPoint, got {other:?}"),
        }
    }

    #[test]
    fn assemble_linestring_roundtrip() {
        let z = 12;
        let (x, y) = (1205, 1539);
        let b = tile_bounds(x, y, z);
        let w = b.lng_max - b.lng_min;
        let h = b.lat_max - b.lat_min;
        let line = LineString::from(vec![
            (b.lng_min + 0.1 * w, b.lat_min + 0.1 * h),
            (b.lng_min + 0.5 * w, b.lat_min + 0.7 * h),
            (b.lng_min + 0.9 * w, b.lat_min + 0.2 * h),
        ]);
        let got = roundtrip(&Geometry::LineString(line.clone()), z, x, y).unwrap();
        match got {
            Geometry::LineString(l) => {
                assert_eq!(l.0.len(), 3);
                for (q, p) in l.0.iter().zip(line.0.iter()) {
                    assert_close((q.x, q.y), (p.x, p.y), z, "linestring vertex");
                }
            }
            other => panic!("expected LineString, got {other:?}"),
        }
    }

    #[test]
    fn assemble_multilinestring_roundtrip() {
        let z = 12;
        let (x, y) = (1205, 1539);
        let b = tile_bounds(x, y, z);
        let w = b.lng_max - b.lng_min;
        let h = b.lat_max - b.lat_min;
        let mls = MultiLineString::new(vec![
            LineString::from(vec![
                (b.lng_min + 0.1 * w, b.lat_min + 0.1 * h),
                (b.lng_min + 0.3 * w, b.lat_min + 0.3 * h),
            ]),
            LineString::from(vec![
                (b.lng_min + 0.6 * w, b.lat_min + 0.6 * h),
                (b.lng_min + 0.9 * w, b.lat_min + 0.8 * h),
                (b.lng_min + 0.9 * w, b.lat_min + 0.9 * h),
            ]),
        ]);
        let got = roundtrip(&Geometry::MultiLineString(mls.clone()), z, x, y).unwrap();
        match got {
            Geometry::MultiLineString(l) => {
                assert_eq!(l.0.len(), 2);
                assert_eq!(l.0[0].0.len(), 2);
                assert_eq!(l.0[1].0.len(), 3);
            }
            other => panic!("expected MultiLineString, got {other:?}"),
        }
    }

    #[test]
    fn assemble_polygon_with_hole_roundtrip() {
        let z = 12;
        let (x, y) = (1205, 1539);
        let b = tile_bounds(x, y, z);
        let w = b.lng_max - b.lng_min;
        let h = b.lat_max - b.lat_min;
        let sq = |x0: f64, y0: f64, x1: f64, y1: f64| {
            LineString::from(vec![
                (b.lng_min + x0 * w, b.lat_min + y0 * h),
                (b.lng_min + x1 * w, b.lat_min + y0 * h),
                (b.lng_min + x1 * w, b.lat_min + y1 * h),
                (b.lng_min + x0 * w, b.lat_min + y1 * h),
                (b.lng_min + x0 * w, b.lat_min + y0 * h),
            ])
        };
        let poly = Polygon::new(sq(0.1, 0.1, 0.9, 0.9), vec![sq(0.4, 0.4, 0.6, 0.6)]);
        let got = roundtrip(&Geometry::Polygon(poly), z, x, y).unwrap();
        match got {
            Geometry::Polygon(p) => {
                assert_eq!(p.exterior().0.len(), 5, "exterior ring closed, 4 corners");
                assert_eq!(p.interiors().len(), 1, "hole preserved");
                assert_eq!(p.interiors()[0].0.len(), 5);
                // Spot-check one corner within tolerance.
                let want = (b.lng_min + 0.1 * w, b.lat_min + 0.1 * h);
                let found = p
                    .exterior()
                    .0
                    .iter()
                    .map(|c| (c.x - want.0).abs().max((c.y - want.1).abs()))
                    .fold(f64::INFINITY, f64::min);
                assert!(found <= lon_unit(z, 4096), "corner error {found}");
            }
            other => panic!("expected Polygon, got {other:?}"),
        }
    }

    #[test]
    fn assemble_multipolygon_roundtrip() {
        let z = 12;
        let (x, y) = (1205, 1539);
        let b = tile_bounds(x, y, z);
        let w = b.lng_max - b.lng_min;
        let h = b.lat_max - b.lat_min;
        let sq = |x0: f64, y0: f64, x1: f64, y1: f64| {
            Polygon::new(
                LineString::from(vec![
                    (b.lng_min + x0 * w, b.lat_min + y0 * h),
                    (b.lng_min + x1 * w, b.lat_min + y0 * h),
                    (b.lng_min + x1 * w, b.lat_min + y1 * h),
                    (b.lng_min + x0 * w, b.lat_min + y1 * h),
                    (b.lng_min + x0 * w, b.lat_min + y0 * h),
                ]),
                vec![],
            )
        };
        let mp = MultiPolygon::new(vec![sq(0.1, 0.1, 0.3, 0.3), sq(0.6, 0.6, 0.9, 0.9)]);
        let got = roundtrip(&Geometry::MultiPolygon(mp), z, x, y).unwrap();
        match got {
            Geometry::MultiPolygon(m) => assert_eq!(m.0.len(), 2),
            other => panic!("expected MultiPolygon, got {other:?}"),
        }
    }

    // ---- Degenerate content -----------------------------------------------

    #[test]
    fn assemble_empty_stream_is_none() {
        for gt in [GeomType::Point, GeomType::Linestring, GeomType::Polygon] {
            assert!(assemble_geometry(gt, &[], |x, y| (x as f64, y as f64)).is_none());
        }
    }

    #[test]
    fn assemble_unknown_type_is_none() {
        let parts = parse_command_stream(&[command_encode(1, 1), 0, 0]).unwrap();
        assert!(
            assemble_geometry(GeomType::Unknown, &parts, |x, y| (x as f64, y as f64)).is_none()
        );
    }

    #[test]
    fn assemble_single_vertex_linestring_is_dropped() {
        // MoveTo(1) with no LineTo: not a valid linestring part.
        let parts =
            parse_command_stream(&[command_encode(1, 1), zigzag_encode(10), zigzag_encode(10)])
                .unwrap();
        assert!(
            assemble_geometry(GeomType::Linestring, &parts, |x, y| (x as f64, y as f64)).is_none()
        );
    }

    #[test]
    fn assemble_unclosed_ring_is_dropped() {
        // A polygon "ring" without ClosePath is degenerate content.
        let geom = [
            command_encode(1, 1),
            zigzag_encode(0),
            zigzag_encode(0),
            command_encode(2, 2),
            zigzag_encode(10),
            zigzag_encode(0),
            zigzag_encode(0),
            zigzag_encode(10),
            // no ClosePath
        ];
        let parts = parse_command_stream(&geom).unwrap();
        assert!(
            assemble_geometry(GeomType::Polygon, &parts, |x, y| (x as f64, y as f64)).is_none()
        );
    }

    #[test]
    fn assemble_zero_area_ring_is_dropped() {
        // Three collinear points closed into a "ring": area 0.
        let geom = [
            command_encode(1, 1),
            zigzag_encode(0),
            zigzag_encode(0),
            command_encode(2, 2),
            zigzag_encode(5),
            zigzag_encode(0),
            zigzag_encode(5),
            zigzag_encode(0),
            command_encode(7, 1),
        ];
        let parts = parse_command_stream(&geom).unwrap();
        assert!(
            assemble_geometry(GeomType::Polygon, &parts, |x, y| (x as f64, y as f64)).is_none()
        );
    }

    #[test]
    fn assemble_leading_interior_ring_is_dropped() {
        // A CCW-in-tile-space (negative-area) ring with no preceding
        // exterior: dropped, not promoted.
        let geom = [
            command_encode(1, 1),
            zigzag_encode(0),
            zigzag_encode(0),
            command_encode(2, 3),
            // (0,0) -> (0,10) -> (10,10) -> (10,0): negative surveyor area
            zigzag_encode(0),
            zigzag_encode(10),
            zigzag_encode(10),
            zigzag_encode(0),
            zigzag_encode(0),
            zigzag_encode(-10),
            command_encode(7, 1),
        ];
        let parts = parse_command_stream(&geom).unwrap();
        assert!(parts[0].area2() < 0, "test ring must be interior-wound");
        assert!(
            assemble_geometry(GeomType::Polygon, &parts, |x, y| (x as f64, y as f64)).is_none()
        );
    }

    // ---- Property decode + type unification --------------------------------

    fn mvt_value() -> crate::vector_tile::tile::Value {
        crate::vector_tile::tile::Value::default()
    }

    #[test]
    fn prop_value_decodes_every_mvt_variant() {
        let mut v = mvt_value();
        v.string_value = Some("hi".into());
        assert_eq!(PropValue::from_mvt(&v), Some(PropValue::Str("hi".into())));

        let mut v = mvt_value();
        v.float_value = Some(1.5);
        assert_eq!(PropValue::from_mvt(&v), Some(PropValue::Float(1.5)));

        let mut v = mvt_value();
        v.double_value = Some(2.5);
        assert_eq!(PropValue::from_mvt(&v), Some(PropValue::Float(2.5)));

        let mut v = mvt_value();
        v.int_value = Some(-7);
        assert_eq!(PropValue::from_mvt(&v), Some(PropValue::Int(-7)));

        let mut v = mvt_value();
        v.sint_value = Some(-9);
        assert_eq!(PropValue::from_mvt(&v), Some(PropValue::Int(-9)));

        let mut v = mvt_value();
        v.uint_value = Some(42);
        assert_eq!(PropValue::from_mvt(&v), Some(PropValue::Int(42)));

        // uint that does not fit i64 degrades to float.
        let mut v = mvt_value();
        v.uint_value = Some(u64::MAX);
        assert_eq!(
            PropValue::from_mvt(&v),
            Some(PropValue::Float(u64::MAX as f64))
        );

        let mut v = mvt_value();
        v.bool_value = Some(true);
        assert_eq!(PropValue::from_mvt(&v), Some(PropValue::Bool(true)));

        // No field set: invalid, skipped.
        assert_eq!(PropValue::from_mvt(&mvt_value()), None);
    }

    #[test]
    fn coltype_unify_lattice() {
        use ColType::*;
        assert_eq!(Int.unify(Int), Int);
        assert_eq!(Int.unify(Float), Float);
        assert_eq!(Float.unify(Int), Float);
        assert_eq!(Bool.unify(Bool), Bool);
        assert_eq!(Str.unify(Str), Str);
        // Any other mixture degrades to string.
        assert_eq!(Bool.unify(Int), Str);
        assert_eq!(Int.unify(Str), Str);
        assert_eq!(Str.unify(Float), Str);
        assert_eq!(Bool.unify(Str), Str);
    }
}
