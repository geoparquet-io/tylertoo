//! Batch PMTiles export from a GeoParquet overview file (Plan E0).
//!
//! This is the **replacement** for the shelved tile-generation pipeline, not a
//! revival of it. An overview file already contains per-level thinned,
//! simplified, ranked, Hilbert-ordered features; exporting to PMTiles is
//! therefore mechanical and single-pass:
//!
//! 1. For each overview **level**, resolve its Web Mercator **zoom** (§5.2).
//! 2. Stream the level band via [`OverviewReader::read_level`] with `bbox=None`.
//!    The reader already implements the mode semantics — `duplicating` reads
//!    exactly the level's own row-group band, `partitioning` reads the prefix
//!    `0..=level` (features accumulate) — so this module treats both modes
//!    identically: "read level `k`, emit tiles at level `k`'s zoom".
//! 3. Assign every feature to the tile(s) it intersects at that zoom
//!    ([`tiles_for_bbox`]), clip each to the tile bounds **plus a pixel buffer**
//!    (reusing the shelved [`clip_geometry`] entry point), MVT-encode
//!    (reusing [`crate::mvt`]), and write via [`StreamingPmtilesWriter`].
//!
//! ## What this deliberately does NOT do (per `context/CARRYOVER.md`)
//!
//! - **No global cross-zoom external sort / per-tile fan-out.** Tiling is done
//!   one zoom at a time into an in-memory `BTreeMap<tile, Vec<feature>>` that is
//!   built, drained into the writer, and dropped before the next zoom.
//! - **No per-tile budget retry loop / adaptive re-encode.** Generalization is
//!   precomputed in the overview file. The only safety valve is a single,
//!   optional, non-iterative drop pass for pathologically dense tiles
//!   (see [`ExportOptions::tile_size_limit`]).
//!
//! ## Memory ceiling
//!
//! For each zoom the peak working set is `O(F_level + C)` where `F_level` is the
//! feature count of that level's band and `C` is the number of
//! (feature × intersecting-tile) clipped copies at that zoom. For the finest
//! (canonical) level in duplicating mode this equals the whole dataset plus its
//! tile-boundary duplication — the documented v1 ceiling. There is **no**
//! global (all-zoom) accumulation.
//!
//! ## Tile-boundary duplication (expected MVT semantics)
//!
//! The overview *format* stores each feature once per level (no clipping). The
//! PMTiles *export* necessarily reintroduces the classic MVT behaviour: a
//! feature spanning a tile seam is clipped into — and therefore appears in —
//! every tile it touches. Per-zoom exported feature totals will therefore be
//! `>=` the overview level's feature count, the excess being border
//! duplication.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type,
    UInt64Type, UInt8Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType, Schema};
use geo::{BoundingRect, CoordsIter, Geometry, MapCoords};
use geoarrow::array::from_arrow_array;
use geoarrow_array::GeoArrowArray;
use prost::Message;
use rayon::prelude::*;
use serde::Serialize;

use crate::batch_processor::extract_geometries_from_array;
use crate::clip::clip_geometry;
use crate::compression::Compression;
use crate::mvt::{LayerBuilder, PropertyValue, TileBuilder};
use crate::pmtiles_writer::StreamingPmtilesWriter;
use crate::tile::{tiles_for_bbox, TileBounds, TileCoord};

use super::level::{zoom_for_gsd, Crs, OverviewsMeta};
use super::reader::{OverviewReader, ReaderError};
use super::writer::LEVEL_COLUMN;

/// Default MVT tile extent (matches [`crate::mvt::DEFAULT_EXTENT`]).
const DEFAULT_EXTENT: u32 = 4096;

/// Default per-tile edge buffer, in tile pixels (tippecanoe default is 5; we
/// use 8 to match the tile pipeline's historical default).
const DEFAULT_TILE_BUFFER_PX: u32 = 8;

/// Web Mercator projected half-extent in meters (EPSG:3857 axis range is
/// `±WEBMERC_HALF_M`). Used only to reproject a 3857 overview to lon/lat so the
/// (geographic) tile grid math applies.
const WEBMERC_HALF_M: f64 = 20_037_508.342_789_244;

/// Options controlling a PMTiles export.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// MVT layer name written into every tile and the archive metadata.
    pub layer_name: String,
    /// Per-tile edge buffer in **tile pixels**. Converted to coordinate units
    /// per tile (`buffer_deg = tile_width * buffer_px / extent`) and applied as
    /// the clip margin, so features spanning a seam render continuously.
    pub tile_buffer: u32,
    /// MVT tile extent (integer tile-local resolution). Default 4096.
    pub extent: u32,
    /// Optional per-tile MVT size limit in **bytes**. When set, a tile whose
    /// encoded size exceeds the limit triggers the single, non-iterative safety
    /// valve: its lowest-priority features (ranked by geometry size — see the
    /// module docs on why the assignment sort key is not recoverable per-row)
    /// are dropped in one pass and the tile is re-encoded once. When `None`, no
    /// size limit is enforced and `oversized_tiles` is always 0.
    pub tile_size_limit: Option<usize>,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            layer_name: "overview".to_string(),
            tile_buffer: DEFAULT_TILE_BUFFER_PX,
            extent: DEFAULT_EXTENT,
            tile_size_limit: None,
        }
    }
}

/// Per-zoom statistics in an [`ExportReport`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ZoomReport {
    /// Web Mercator zoom this level was exported at.
    pub zoom: u8,
    /// Overview level index that produced this zoom.
    pub level: usize,
    /// Overview level feature count (rows read from the band).
    pub level_feature_count: usize,
    /// Number of tiles written at this zoom.
    pub tile_count: usize,
    /// Total features written across all tiles at this zoom (>= level feature
    /// count due to tile-boundary duplication).
    pub tile_feature_count: usize,
    /// Number of tiles at this zoom that hit the oversized safety valve.
    pub oversized_tiles: usize,
}

/// Result of an export, `Serialize` for the `--report` JSON.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExportReport {
    /// Level materialization mode of the source overview file.
    pub mode: String,
    /// PMTiles header min zoom (coarsest level's zoom).
    pub min_zoom: u8,
    /// PMTiles header max zoom (finest level's zoom).
    pub max_zoom: u8,
    /// Per-zoom statistics, coarse→fine.
    pub zooms: Vec<ZoomReport>,
    /// Total tiles written across all zooms.
    pub total_tiles: usize,
    /// Total features written across all tiles (with border duplication).
    pub total_tile_features: usize,
    /// Total tiles that hit the oversized safety valve.
    pub oversized_tiles: usize,
    /// Wall-clock export duration in seconds.
    pub duration_secs: f64,
}

/// Errors from [`export_pmtiles`].
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// Opening or reading the overview file failed.
    #[error("overview reader error: {0}")]
    Reader(#[from] ReaderError),
    /// I/O error (writing the PMTiles archive).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Arrow error decoding a level batch.
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// A core-library error (geometry decode, PMTiles write).
    #[error("{0}")]
    Core(#[from] crate::Error),
    /// The input CRS is neither EPSG:4326 nor EPSG:3857.
    #[error("unsupported input CRS {crs:?}: export requires EPSG:4326 or EPSG:3857")]
    UnsupportedCrs {
        /// The rejected CRS identifier.
        crs: String,
    },
    /// The file has no geometry column.
    #[error("overview file has no geometry column")]
    NoGeometryColumn,
}

/// One source feature: its (possibly reprojected-to-4326) geometry and the MVT
/// properties carried over from the overview file (level column + covering
/// struct excluded).
struct Feature {
    geom: Geometry<f64>,
    props: Vec<(String, PropertyValue)>,
}

/// An encoded tile ready to hand to the PMTiles writer.
struct EncodedTile {
    x: u32,
    y: u32,
    data: Vec<u8>,
    feature_count: usize,
    oversized: bool,
}

/// Resolve the Web Mercator zoom for overview level `level_idx`.
///
/// Uses the level's explicit `zoom` when present (§3.2). When absent, derives it
/// from the level GSD via the §5.2 inverse — `z = round(log2(C / base / gsd))` —
/// and clamps to `u8`. The rounding rule (nearest integer) is documented here so
/// the mapping is reproducible: a level whose GSD sits between two zooms maps to
/// the nearer one.
pub fn zoom_for_level(meta: &OverviewsMeta, level_idx: usize) -> u8 {
    let level = &meta.levels[level_idx];
    if let Some(z) = level.zoom {
        return z;
    }
    let z = zoom_for_gsd(level.gsd).round();
    z.clamp(0.0, 255.0) as u8
}

/// Export an overview GeoParquet file to a PMTiles archive.
///
/// See the module documentation for the full pipeline and the design
/// constraints it observes.
pub fn export_pmtiles(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &ExportOptions,
) -> Result<ExportReport, ExportError> {
    let start = Instant::now();
    let input_path = input_path.as_ref();

    let reader = OverviewReader::open(input_path)?;
    let meta = reader.meta().clone();
    let crs = detect_crs(input_path)?;

    let num_levels = reader.num_levels();
    let min_zoom = zoom_for_level(&meta, 0);
    let max_zoom = zoom_for_level(&meta, num_levels - 1);

    // Writer: field metadata + layer name are derived from the first level's
    // schema (property columns, level/covering excluded).
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip)?;
    writer.set_layer_name(&options.layer_name);
    writer.set_fields(field_metadata(
        reader.schema(),
        geometry_index(reader.schema()),
    ));

    let mut zooms: Vec<ZoomReport> = Vec::with_capacity(num_levels);
    let mut overall_bounds: Option<TileBounds> = None;

    for level_idx in 0..num_levels {
        let zoom = zoom_for_level(&meta, level_idx);

        // Read the whole level band into memory (v1). The reader already applies
        // duplicating (single band) vs partitioning (prefix) semantics.
        let t_read = Instant::now();
        let features = read_level_features(&reader, level_idx, crs)?;
        let read_secs = t_read.elapsed().as_secs_f64();
        let level_feature_count = features.len();

        // Expand the overall geographic bounds from feature bboxes.
        for f in &features {
            if let Some(rect) = f.geom.bounding_rect() {
                let b = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
                match &mut overall_bounds {
                    Some(acc) => acc.expand(&b),
                    None => overall_bounds = Some(b),
                }
            }
        }

        let t_encode = Instant::now();
        let tiles = encode_level_tiles(&features, zoom, options);
        let encode_secs = t_encode.elapsed().as_secs_f64();

        let t_write = Instant::now();
        let mut tile_feature_count = 0usize;
        let mut oversized = 0usize;
        for t in &tiles {
            tile_feature_count += t.feature_count;
            if t.oversized {
                oversized += 1;
            }
            writer.add_tile_with_count(zoom, t.x, t.y, &t.data, t.feature_count)?;
        }
        log::debug!(
            "[profile] z{zoom} (level {level_idx}, {level_feature_count} feats, {} tiles): \
             read={read_secs:.2}s clip+encode={encode_secs:.2}s write(gzip)={:.2}s",
            tiles.len(),
            t_write.elapsed().as_secs_f64(),
        );

        zooms.push(ZoomReport {
            zoom,
            level: level_idx,
            level_feature_count,
            tile_count: tiles.len(),
            tile_feature_count,
            oversized_tiles: oversized,
        });
    }

    if let Some(b) = &overall_bounds {
        writer.set_bounds(b);
    }
    let t_finalize = Instant::now();
    writer.finalize(output_path.as_ref())?;
    log::debug!(
        "[profile] pmtiles finalize: {:.2}s",
        t_finalize.elapsed().as_secs_f64()
    );

    let total_tiles = zooms.iter().map(|z| z.tile_count).sum();
    let total_tile_features = zooms.iter().map(|z| z.tile_feature_count).sum();
    let oversized_tiles = zooms.iter().map(|z| z.oversized_tiles).sum();

    Ok(ExportReport {
        mode: format!("{:?}", reader.mode()).to_lowercase(),
        min_zoom,
        max_zoom,
        zooms,
        total_tiles,
        total_tile_features,
        oversized_tiles,
        duration_secs: start.elapsed().as_secs_f64(),
    })
}

// ============================================================================
// Tiling + encoding
// ============================================================================

/// One tile's members: list of `(clipped geometry, property index into the
/// level's `features`)`.
type TileMembers = Vec<(Geometry<f64>, usize)>;

/// Per-tile working map: tile `(x, y)` -> members.
type GroupedTileGeoms = BTreeMap<(u32, u32), TileMembers>;

/// Per-feature clip results: list of `(tile (x, y), clipped geometry)`.
type FeatureTileGeoms = Vec<((u32, u32), Geometry<f64>)>;

/// Group a level's features into tiles at `zoom`, clip each to its tile bounds
/// plus the pixel buffer, and MVT-encode. Applies the optional oversized valve.
///
/// Returns the encoded tiles in Hilbert-ish `(x, y)` `BTreeMap` order (the
/// PMTiles writer re-sorts by tile id regardless, so exact order is not
/// load-bearing — the `BTreeMap` is used for deterministic, locality-friendly
/// grouping and a bounded per-zoom working set).
fn encode_level_tiles(features: &[Feature], zoom: u8, opts: &ExportOptions) -> Vec<EncodedTile> {
    // Clip in parallel per feature (H3(c) lever 2: clipping is 94% of export
    // wall and independent per feature). `par_iter().map().collect()`
    // preserves feature order, so the sequential merge below pushes members
    // into each tile's vector in exactly the serial order — grouping, and
    // therefore the encoded tile bytes, are unchanged.
    let t_clip = Instant::now();
    let per_feature: Vec<FeatureTileGeoms> = features
        .par_iter()
        .map(|feat| {
            let Some(rect) = feat.geom.bounding_rect() else {
                return Vec::new();
            };
            let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
            let mut out = Vec::new();
            for tc in tiles_for_bbox(&bbox, zoom) {
                let tb = tc.bounds();
                let buffer_deg = tb.width() * opts.tile_buffer as f64 / opts.extent as f64;
                // Fast path (H3(c) lever 4): a feature whose bbox lies entirely
                // within the buffered tile bounds is unaffected by clipping —
                // skip the BooleanOps intersection and emit the geometry as-is.
                // At z14 ~80% of features are interior to a single tile.
                if bbox_within_buffered(&bbox, &tb, buffer_deg) {
                    out.push(((tc.x, tc.y), feat.geom.clone()));
                } else if let Some(clipped) = clip_geometry(&feat.geom, &tb, buffer_deg) {
                    out.push(((tc.x, tc.y), clipped));
                }
            }
            out
        })
        .collect();

    // tile (x,y) -> list of (clipped geometry, property index into `features`).
    let mut grouped: GroupedTileGeoms = BTreeMap::new();
    for (fi, items) in per_feature.into_iter().enumerate() {
        for (key, geom) in items {
            grouped.entry(key).or_default().push((geom, fi));
        }
    }

    let clip_secs = t_clip.elapsed().as_secs_f64();

    // Encode tiles in parallel; the indexed collect preserves the BTreeMap's
    // (x, y) order, and the caller's serial write loop consumes `out` in that
    // order, so PMTiles tile ordering and bytes are unchanged.
    let t_mvt = Instant::now();
    let entries: Vec<((u32, u32), TileMembers)> = grouped.into_iter().collect();
    let out: Vec<EncodedTile> = entries
        .into_par_iter()
        .filter_map(|((x, y), members)| {
            let tc = TileCoord::new(x, y, zoom);
            let tb = tc.bounds();
            let (data, count, oversized) = encode_tile(&members, features, &tb, opts);
            if count == 0 {
                return None;
            }
            Some(EncodedTile {
                x,
                y,
                data,
                feature_count: count,
                oversized,
            })
        })
        .collect();
    log::debug!(
        "[profile]   z{zoom} clip+group={clip_secs:.2}s mvt-encode={:.2}s",
        t_mvt.elapsed().as_secs_f64(),
    );
    out
}

/// `true` when `bbox` lies entirely within `tb` expanded by `buffer` on every
/// side. Clipping such a feature to the buffered tile is a geometric no-op, so
/// the clip can be skipped (and BooleanOps ring normalization avoided).
#[inline]
fn bbox_within_buffered(bbox: &TileBounds, tb: &TileBounds, buffer: f64) -> bool {
    bbox.lng_min >= tb.lng_min - buffer
        && bbox.lat_min >= tb.lat_min - buffer
        && bbox.lng_max <= tb.lng_max + buffer
        && bbox.lat_max <= tb.lat_max + buffer
}

/// Encode a single tile's members to MVT bytes, applying the oversized valve.
/// Returns `(bytes, features_encoded, oversized)`.
fn encode_tile(
    members: &[(Geometry<f64>, usize)],
    features: &[Feature],
    tb: &TileBounds,
    opts: &ExportOptions,
) -> (Vec<u8>, usize, bool) {
    let data = build_mvt(members, features, tb, opts);

    match opts.tile_size_limit {
        Some(limit) if data.len() > limit && members.len() > 1 => {
            // Single, non-iterative drop pass: rank members by geometry size
            // (coordinate count) descending — the biggest features carry the
            // tile's visual signal — and keep a proportional prefix.
            let mut ranked: Vec<&(Geometry<f64>, usize)> = members.iter().collect();
            ranked.sort_by_key(|m| std::cmp::Reverse(m.0.coords_count()));
            let keep_frac = limit as f64 / data.len() as f64;
            let keep = ((members.len() as f64 * keep_frac).floor() as usize).max(1);
            let kept: Vec<(Geometry<f64>, usize)> =
                ranked.into_iter().take(keep).cloned().collect();
            let data = build_mvt(&kept, features, tb, opts);
            let count = kept.len();
            log::warn!(
                "oversized tile ({} bytes > {limit} limit): dropped {} of {} features (one pass)",
                data.len(),
                members.len() - count,
                members.len()
            );
            (data, count, true)
        }
        _ => {
            let count = members.len();
            (data, count, false)
        }
    }
}

/// Build the MVT bytes for a set of tile members.
fn build_mvt(
    members: &[(Geometry<f64>, usize)],
    features: &[Feature],
    tb: &TileBounds,
    opts: &ExportOptions,
) -> Vec<u8> {
    let mut layer = LayerBuilder::new(opts.layer_name.clone()).with_extent(opts.extent);
    for (i, (geom, fi)) in members.iter().enumerate() {
        layer.add_feature(Some(i as u64), geom, &features[*fi].props, tb);
    }
    let mut tb_builder = TileBuilder::new();
    tb_builder.add_layer(layer.build());
    tb_builder.build().encode_to_vec()
}

// ============================================================================
// Reading + property extraction
// ============================================================================

/// Read every feature (geometry + carried properties) of a level band.
fn read_level_features(
    reader: &OverviewReader,
    level_idx: usize,
    crs: Crs,
) -> Result<Vec<Feature>, ExportError> {
    let batch_reader = reader.read_level(level_idx, None)?;
    let mut out = Vec::new();
    for batch in batch_reader {
        let batch = batch?;
        decode_batch(&batch, crs, &mut out)?;
    }
    Ok(out)
}

/// Decode one record batch into [`Feature`]s, excluding the `level` column and
/// any struct/list column (the bbox covering) from properties.
fn decode_batch(batch: &RecordBatch, crs: Crs, out: &mut Vec<Feature>) -> Result<(), ExportError> {
    let schema = batch.schema();
    let geom_idx = geometry_index(&schema).ok_or(ExportError::NoGeometryColumn)?;
    let geom_field = schema.field(geom_idx).clone();

    // Decode geometries.
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(garr.as_ref(), &mut geoms)?;

    // Pre-extract every exportable property column once.
    let prop_cols = property_columns(&schema, geom_idx);
    let mut extracted: Vec<(String, Vec<Option<PropertyValue>>)> =
        Vec::with_capacity(prop_cols.len());
    for &(idx, ref name) in &prop_cols {
        extracted.push((name.clone(), extract_property_column(batch.column(idx))));
    }

    for row in 0..batch.num_rows() {
        let mut geom = geoms[row].clone();
        if matches!(crs, Crs::Epsg3857) {
            geom = reproject_3857_to_4326(&geom);
        }
        let mut props = Vec::with_capacity(extracted.len());
        for (name, col) in &extracted {
            if let Some(v) = &col[row] {
                props.push((name.clone(), v.clone()));
            }
        }
        out.push(Feature { geom, props });
    }
    Ok(())
}

/// The index of the primary geometry column (`geometry`, else first `geom*`).
fn geometry_index(schema: &Schema) -> Option<usize> {
    schema
        .fields()
        .iter()
        .position(|f| f.name() == "geometry")
        .or_else(|| {
            schema
                .fields()
                .iter()
                .position(|f| f.name().contains("geom"))
        })
}

/// The `(index, name)` of every exportable property column: everything that is
/// not the geometry column, not the `level` column, and whose type is a
/// supported MVT scalar (struct/list covering columns are skipped).
fn property_columns(schema: &Schema, geom_idx: usize) -> Vec<(usize, String)> {
    schema
        .fields()
        .iter()
        .enumerate()
        .filter(|&(i, f)| {
            i != geom_idx
                && !f.name().eq_ignore_ascii_case(LEVEL_COLUMN)
                && is_supported_scalar(f.data_type())
        })
        .map(|(i, f)| (i, f.name().clone()))
        .collect()
}

/// MVT-encodable Arrow scalar types.
fn is_supported_scalar(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
    )
}

/// Field-type metadata (`name -> "String"|"Number"|"Boolean"`) for the archive
/// `vector_layers.fields` block.
fn field_metadata(schema: &Schema, geom_idx: Option<usize>) -> HashMap<String, String> {
    let geom_idx = geom_idx.unwrap_or(usize::MAX);
    let mut out = HashMap::new();
    for (i, f) in schema.fields().iter().enumerate() {
        if i == geom_idx || f.name().eq_ignore_ascii_case(LEVEL_COLUMN) {
            continue;
        }
        let ty = match f.data_type() {
            DataType::Utf8 | DataType::LargeUtf8 => "String",
            DataType::Boolean => "Boolean",
            dt if is_supported_scalar(dt) => "Number",
            _ => continue,
        };
        out.insert(f.name().clone(), ty.to_string());
    }
    out
}

/// Extract one Arrow column into per-row optional [`PropertyValue`]s. Null cells
/// and unsupported types yield `None`.
fn extract_property_column(col: &dyn arrow_array::Array) -> Vec<Option<PropertyValue>> {
    let n = col.len();
    macro_rules! prim {
        ($ty:ty, $variant:ident, $cast:ty) => {{
            let a = col.as_primitive::<$ty>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::$variant(a.value(i) as $cast))
                    }
                })
                .collect()
        }};
    }
    macro_rules! strcol {
        ($off:ty) => {{
            let a = col.as_string::<$off>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::String(a.value(i).to_string()))
                    }
                })
                .collect()
        }};
    }
    match col.data_type() {
        DataType::Utf8 => strcol!(i32),
        DataType::LargeUtf8 => strcol!(i64),
        DataType::Boolean => {
            let a = col.as_boolean();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::Bool(a.value(i)))
                    }
                })
                .collect()
        }
        DataType::Int8 => prim!(Int8Type, Int, i64),
        DataType::Int16 => prim!(Int16Type, Int, i64),
        DataType::Int32 => prim!(Int32Type, Int, i64),
        DataType::Int64 => prim!(Int64Type, Int, i64),
        DataType::UInt8 => prim!(UInt8Type, UInt, u64),
        DataType::UInt16 => prim!(UInt16Type, UInt, u64),
        DataType::UInt32 => prim!(UInt32Type, UInt, u64),
        DataType::UInt64 => prim!(UInt64Type, UInt, u64),
        DataType::Float32 => prim!(Float32Type, Float, f32),
        DataType::Float64 => prim!(Float64Type, Double, f64),
        _ => vec![None; n],
    }
}

// ============================================================================
// CRS
// ============================================================================

/// Detect the overview file's CRS. A null/absent CRS is GeoParquet's default
/// EPSG:4326. EPSG:3857 is accepted (reprojected on read); anything else errors.
fn detect_crs(path: &Path) -> Result<Crs, ExportError> {
    let info = crate::quality::extract_crs(path).map_err(ExportError::Core)?;
    if info.is_wgs84 {
        return Ok(Crs::Epsg4326);
    }
    if let Some(id) = &info.identifier {
        let up = id.to_uppercase();
        if up.contains("3857") || up.contains("900913") {
            return Ok(Crs::Epsg3857);
        }
    }
    // A null CRS (identifier None, name None) is the GeoParquet default 4326.
    if info.identifier.is_none() && info.name.is_none() {
        return Ok(Crs::Epsg4326);
    }
    Err(ExportError::UnsupportedCrs {
        crs: info
            .identifier
            .or(info.name)
            .unwrap_or_else(|| "unknown".to_string()),
    })
}

/// Reproject a geometry from EPSG:3857 (meters) to EPSG:4326 (lon/lat degrees)
/// so the geographic tile grid math applies.
fn reproject_3857_to_4326(g: &Geometry<f64>) -> Geometry<f64> {
    g.map_coords(|c| {
        let lng = c.x / WEBMERC_HALF_M * 180.0;
        let lat = (2.0 * (c.y / WEBMERC_HALF_M * std::f64::consts::PI).exp().atan()
            - std::f64::consts::FRAC_PI_2)
            .to_degrees();
        geo::coord! { x: lng, y: lat }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{command_decode, zigzag_decode};
    use crate::overview::level::{gsd, Level, Mode};
    use crate::overview::writer::{LevelSpec, OverviewWriter, OverviewWriterOptions};
    use crate::vector_tile::tile::GeomType;
    use crate::vector_tile::Tile;
    use arrow_array::{Int64Array, RecordBatch, StringArray};
    use arrow_schema::{Field, Schema};
    use geo::{Geometry, LineString, Point};
    use geoarrow::array::GeometryBuilder;
    use geoarrow::datatypes::GeometryType;

    // --- fixture builders ----------------------------------------------------

    fn build_geometry_array(geoms: &[Geometry<f64>]) -> geoarrow::array::GeometryArray {
        let typ = GeometryType::new(Default::default());
        let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
        b.extend_from_iter(geoms.iter().map(Some));
        b.finish()
    }

    fn geometry_field() -> Field {
        build_geometry_array(&[Geometry::Point(Point::new(0.0, 0.0))])
            .data_type()
            .to_field("geometry", true)
    }

    fn source_schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            geometry_field(),
        ])
    }

    fn batch(schema: &Arc<Schema>, ids: &[i64], geoms: &[Geometry<f64>]) -> RecordBatch {
        let id = Int64Array::from(ids.to_vec());
        let name = StringArray::from(ids.iter().map(|i| format!("f{i}")).collect::<Vec<_>>());
        let geom = build_geometry_array(geoms);
        RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(id), Arc::new(name), Arc::new(geom.to_array_ref())],
        )
        .unwrap()
    }

    /// Write a 2-level duplicating overview fixture. `level_geoms[k]` are the
    /// geometries (and ids) at level k. Levels use z = 2 + 2k.
    fn write_fixture(path: &Path, level_geoms: &[(Vec<i64>, Vec<Geometry<f64>>)]) -> OverviewsMeta {
        let schema = Arc::new(source_schema());
        let specs: Vec<LevelSpec> = (0..level_geoms.len())
            .map(|k| LevelSpec::new(gsd((2 + 2 * k) as u8), Some((2 + 2 * k) as u8)))
            .collect();
        let mut opts = OverviewWriterOptions::new(Mode::Duplicating, specs);
        opts.max_row_group_size = 10_000;
        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        for (k, (ids, geoms)) in level_geoms.iter().enumerate() {
            writer
                .write_level(
                    k,
                    Some(ids.len()),
                    std::iter::once(batch(&schema, ids, geoms)),
                )
                .unwrap();
        }
        writer.finish().unwrap()
    }

    /// Decode all (geom_type, coords, keys) of a raw MVT tile's single layer.
    fn decode_tile(bytes: &[u8]) -> Tile {
        Tile::decode(bytes).unwrap()
    }

    /// Decode a feature's absolute tile-local coordinates from its command stream.
    fn decode_coords(geom: &[u32]) -> Vec<(i32, i32)> {
        let mut coords = Vec::new();
        let (mut cx, mut cy) = (0i32, 0i32);
        let mut i = 0;
        while i < geom.len() {
            let (cmd, count) = command_decode(geom[i]);
            i += 1;
            if cmd == 7 {
                // ClosePath: no params.
                continue;
            }
            for _ in 0..count {
                cx += zigzag_decode(geom[i]);
                cy += zigzag_decode(geom[i + 1]);
                coords.push((cx, cy));
                i += 2;
            }
        }
        coords
    }

    // --- tests ---------------------------------------------------------------

    /// Rich 2-level fixture used by the byte-equivalence tests: points, a
    /// seam-crossing line, a concave polygon and a many-vertex line, with
    /// id/name properties, at zooms 2 and 4.
    fn equivalence_fixture(path: &Path) {
        let pa = Geometry::Point(Point::new(-120.0, 40.0));
        let pb = Geometry::Point(Point::new(120.0, -40.0));
        let wide = Geometry::LineString(LineString::from(vec![
            (-100.0, 10.0),
            (-80.0, 12.0),
            (-60.0, 8.0),
            (-40.0, 11.0),
        ]));
        let concave = Geometry::Polygon(geo::Polygon::new(
            LineString::from(vec![
                (-100.0, 25.0),
                (-98.0, 25.0),
                (-98.0, 27.0),
                (-99.0, 25.5),
                (-100.0, 27.0),
                (-100.0, 25.0),
            ]),
            vec![],
        ));
        let mut coords = Vec::new();
        for k in 0..200 {
            coords.push((30.0 + k as f64 * 0.3, -20.0 + (k as f64 * 0.1).sin()));
        }
        let wiggly = Geometry::LineString(LineString::from(coords));
        write_fixture(
            path,
            &[
                (vec![0, 1], vec![pa.clone(), pb.clone()]),
                (vec![0, 1, 2, 3, 4], vec![pa, pb, wide, concave, wiggly]),
            ],
        );
    }

    /// Byte-level anchor for the H3(b) export restructure: the whole archive
    /// (header + directory + metadata + tile data) must hash to the value
    /// produced by the pre-refactor per-zoom BTreeMap implementation. The
    /// reference hash was captured from that implementation (commit b8a1635)
    /// on this exact fixture before the partitioned-streaming rewrite.
    #[test]
    fn export_archive_matches_pre_refactor_reference() {
        let tin = tempfile::NamedTempFile::new().unwrap();
        equivalence_fixture(tin.path());
        let tout = tempfile::NamedTempFile::new().unwrap();
        let opts = ExportOptions {
            layer_name: "ref".to_string(),
            ..Default::default()
        };
        let report = export_pmtiles(tin.path(), tout.path(), &opts).unwrap();
        assert_eq!(report.total_tiles, 12);
        let bytes = std::fs::read(tout.path()).unwrap();
        assert_eq!(
            format!("{:016x}", crate::dedup::TileHasher::hash(&bytes)),
            "58d90ae6c69d16f6",
            "archive bytes diverged from the pre-refactor reference"
        );
    }

    #[test]
    fn zoom_mapping_uses_explicit_zoom() {
        let meta = OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(1),
            levels: vec![
                Level {
                    row_group_end: 0,
                    gsd: gsd(4),
                    zoom: Some(4),
                },
                Level {
                    row_group_end: 1,
                    gsd: gsd(7),
                    zoom: Some(7),
                },
            ],
            generalization: None,
        };
        assert_eq!(zoom_for_level(&meta, 0), 4);
        assert_eq!(zoom_for_level(&meta, 1), 7);
    }

    #[test]
    fn zoom_mapping_derives_from_gsd_when_absent() {
        // No zoom on the levels: derive by rounding zoom_for_gsd(gsd).
        let meta = OverviewsMeta {
            version: "0.1.0".to_string(),
            mode: Some(Mode::Duplicating),
            canonical_level: Some(1),
            levels: vec![
                Level {
                    row_group_end: 0,
                    gsd: gsd(3),
                    zoom: None,
                },
                Level {
                    row_group_end: 1,
                    gsd: gsd(8),
                    zoom: None,
                },
            ],
            generalization: None,
        };
        assert_eq!(zoom_for_level(&meta, 0), 3);
        assert_eq!(zoom_for_level(&meta, 1), 8);
    }

    #[test]
    fn partitioning_prefix_semantics_via_reader() {
        // Verify that in partitioning mode, reading level k returns the prefix
        // (accumulating features), which export relies on.
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let specs = vec![
            LevelSpec::new(gsd(4), Some(4)),
            LevelSpec::new(gsd(6), Some(6)),
        ];
        let mut opts = OverviewWriterOptions::new(Mode::Partitioning, specs);
        opts.max_row_group_size = 10_000;
        let mut w = OverviewWriter::create(tmp.path(), &schema, opts).unwrap();
        w.write_level(
            0,
            Some(1),
            std::iter::once(batch(
                &schema,
                &[0],
                &[Geometry::Point(Point::new(1.0, 1.0))],
            )),
        )
        .unwrap();
        w.write_level(
            1,
            Some(1),
            std::iter::once(batch(
                &schema,
                &[1],
                &[Geometry::Point(Point::new(2.0, 2.0))],
            )),
        )
        .unwrap();
        w.finish().unwrap();

        let reader = OverviewReader::open(tmp.path()).unwrap();
        let l0 = read_level_features(&reader, 0, Crs::Epsg4326).unwrap();
        let l1 = read_level_features(&reader, 1, Crs::Epsg4326).unwrap();
        // Level 0 band = {feature 0}. Level 1 = prefix {0,1}.
        assert_eq!(l0.len(), 1);
        assert_eq!(l1.len(), 2);
    }

    #[test]
    fn export_feature_counts_and_level_absent_from_props() {
        // Two features far apart so at a coarse zoom each lands in its own tile.
        let a = Geometry::Point(Point::new(-120.0, 40.0));
        let b = Geometry::Point(Point::new(120.0, -40.0));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            &[
                (vec![0, 1], vec![a.clone(), b.clone()]),
                (vec![0, 1], vec![a.clone(), b.clone()]),
            ],
        );

        let reader = OverviewReader::open(tmp.path()).unwrap();
        let feats = read_level_features(&reader, 0, Crs::Epsg4326).unwrap();
        assert_eq!(feats.len(), 2);

        // Level 0 -> zoom 2. Two far-apart points => two distinct tiles.
        let tiles = encode_level_tiles(&feats, 2, &ExportOptions::default());
        assert_eq!(tiles.len(), 2, "two far-apart points => two tiles");
        for t in &tiles {
            assert_eq!(t.feature_count, 1);
            let decoded = decode_tile(&t.data);
            let layer = &decoded.layers[0];
            assert_eq!(layer.features.len(), 1);
            // `level` column must NOT appear as an MVT property key.
            assert!(
                !layer.keys.iter().any(|k| k.eq_ignore_ascii_case("level")),
                "level column leaked into MVT properties: {:?}",
                layer.keys
            );
            // Carried props: id + name present.
            assert!(layer.keys.iter().any(|k| k == "id"));
            assert!(layer.keys.iter().any(|k| k == "name"));
            assert_eq!(
                decoded.layers[0].features[0].r#type,
                Some(GeomType::Point as i32)
            );
        }
    }

    #[test]
    fn clipped_features_within_tile_plus_buffer() {
        // A line spanning a wide longitude range: at a fine-ish zoom it crosses
        // several tiles and must be clipped into each, with coords bounded by
        // [-buffer, extent+buffer].
        // Span ~60° of longitude so at z4 (22.5°/tile) it crosses several tiles.
        let line = Geometry::LineString(LineString::from(vec![
            (-100.0, 10.0),
            (-80.0, 12.0),
            (-60.0, 8.0),
            (-40.0, 11.0),
        ]));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            &[(vec![0], vec![line.clone()]), (vec![0], vec![line.clone()])],
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        let feats = read_level_features(&reader, 1, Crs::Epsg4326).unwrap();

        let opts = ExportOptions::default();
        // Level 1 -> zoom 4.
        let tiles = encode_level_tiles(&feats, 4, &opts);
        assert!(tiles.len() >= 2, "wide line must span multiple tiles");
        let extent = opts.extent as i32;
        let slack = extent * opts.tile_buffer as i32 / opts.extent as i32 + 4;
        for t in &tiles {
            let decoded = decode_tile(&t.data);
            for f in &decoded.layers[0].features {
                for (x, y) in decode_coords(&f.geometry) {
                    assert!(
                        x >= -slack - extent && x <= extent + slack + extent,
                        "x {x} outside buffered tile bounds"
                    );
                    assert!(
                        y >= -slack - extent && y <= extent + slack + extent,
                        "y {y} outside buffered tile bounds"
                    );
                }
            }
        }
    }

    #[test]
    fn bbox_contained_feature_bypasses_clip_with_identical_output() {
        // A concave polygon fully inside one z4 tile (z4 tile width 22.5°; the
        // tile containing lng -100..., lat ~40 spans well past this 2° shape).
        // The fast path must emit the geometry as-is: the encoded tile bytes
        // must equal an MVT built from the *unclipped* geometry.
        let poly = Geometry::Polygon(geo::Polygon::new(
            LineString::from(vec![
                (-100.0, 25.0),
                (-98.0, 25.0),
                (-98.0, 27.0),
                (-99.0, 25.5), // concavity: BooleanOps clip normalizes rings
                (-100.0, 27.0),
                (-100.0, 25.0),
            ]),
            vec![],
        ));
        let feats = vec![Feature {
            geom: poly.clone(),
            props: vec![],
        }];
        let opts = ExportOptions::default();
        let tiles = encode_level_tiles(&feats, 4, &opts);
        assert_eq!(tiles.len(), 1, "fully-contained feature => exactly 1 tile");
        assert_eq!(tiles[0].feature_count, 1);

        let tc = TileCoord::new(tiles[0].x, tiles[0].y, 4);
        let expected = build_mvt(&[(poly.clone(), 0)], &feats, &tc.bounds(), &opts);
        assert_eq!(
            tiles[0].data, expected,
            "contained feature must bypass the clip (geometry emitted as-is)"
        );
    }

    #[test]
    fn seam_crossing_feature_still_clips() {
        // A line spanning several z4 tiles must still be clipped per tile:
        // it lands in >= 2 tiles and no tile carries the full-extent geometry.
        let line = Geometry::LineString(LineString::from(vec![(-100.0, 10.0), (-40.0, 11.0)]));
        let feats = vec![Feature {
            geom: line,
            props: vec![],
        }];
        let opts = ExportOptions::default();
        let tiles = encode_level_tiles(&feats, 4, &opts);
        assert!(tiles.len() >= 2, "seam-crossing line must span tiles");
        let extent = opts.extent as i32;
        let slack = (opts.tile_buffer as i32) + 4;
        for t in &tiles {
            let decoded = decode_tile(&t.data);
            for f in &decoded.layers[0].features {
                for (x, y) in decode_coords(&f.geometry) {
                    assert!(
                        x >= -slack && x <= extent + slack && y >= -slack && y <= extent + slack,
                        "({x},{y}) outside buffered tile: geometry was not clipped"
                    );
                }
            }
        }
    }

    #[test]
    fn oversized_valve_fires_with_tiny_limit() {
        // Many vertices in one tile; a tiny byte limit forces the single-pass
        // drop valve.
        let mut coords = Vec::new();
        for k in 0..200 {
            coords.push((-100.0 + k as f64 * 0.001, 40.0 + (k as f64 * 0.01).sin()));
        }
        let line = Geometry::LineString(LineString::from(coords));
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Put many copies of the line in one level so a single tile is heavy.
        let geoms: Vec<Geometry<f64>> = (0..20).map(|_| line.clone()).collect();
        let ids: Vec<i64> = (0..20).collect();
        write_fixture(
            tmp.path(),
            &[(ids.clone(), geoms.clone()), (ids.clone(), geoms.clone())],
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        let feats = read_level_features(&reader, 1, Crs::Epsg4326).unwrap();

        // No limit: no oversized tiles.
        let none = encode_level_tiles(&feats, 6, &ExportOptions::default());
        assert!(none.iter().all(|t| !t.oversized));
        let full_count: usize = none.iter().map(|t| t.feature_count).sum();

        // Tiny limit: valve fires and drops features.
        let opts = ExportOptions {
            tile_size_limit: Some(64),
            ..Default::default()
        };
        let limited = encode_level_tiles(&feats, 6, &opts);
        assert!(
            limited.iter().any(|t| t.oversized),
            "tiny --tile-size-limit must trip the valve"
        );
        let limited_count: usize = limited.iter().map(|t| t.feature_count).sum();
        assert!(
            limited_count < full_count,
            "valve must drop features ({limited_count} !< {full_count})"
        );
    }

    #[test]
    fn export_pmtiles_writes_nonempty_archive() {
        let a = Geometry::Point(Point::new(-120.0, 40.0));
        let b = Geometry::Point(Point::new(120.0, -40.0));
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tin.path(),
            &[
                (vec![0, 1], vec![a.clone(), b.clone()]),
                (vec![0, 1], vec![a.clone(), b.clone()]),
            ],
        );
        let tout = tempfile::NamedTempFile::new().unwrap();
        let report = export_pmtiles(tin.path(), tout.path(), &ExportOptions::default()).unwrap();

        assert_eq!(report.min_zoom, 2);
        assert_eq!(report.max_zoom, 4);
        assert_eq!(report.zooms.len(), 2);
        assert!(report.total_tiles >= 2);
        // The archive exists and has a PMTiles header.
        let meta = std::fs::metadata(tout.path()).unwrap();
        assert!(meta.len() > 127, "archive must be larger than the header");
        let mut magic = [0u8; 7];
        use std::io::Read;
        std::fs::File::open(tout.path())
            .unwrap()
            .read_exact(&mut magic)
            .unwrap();
        assert_eq!(&magic, b"PMTiles");
    }
}
