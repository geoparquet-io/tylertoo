//! PMTiles → GeoParquet decoding (issue #112).
//!
//! Decodes a PMTiles v3 vector-tile archive back into a GeoParquet file,
//! following the model of `tippecanoe-decode` (decode.cpp / `write_json.cpp)`:
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
//! # Memory
//!
//! The archive is never loaded whole (#462). The header and directories are
//! read up front; tile bodies are read by offset, through a small read-ahead
//! window, once per pass (the schema pass, then the row pass). Run-length
//! entries are expanded lazily. Peak memory is O(directory entries + one tile
//! + one output batch), however large the archive is.
//!
//! # Coordinate transform
//!
//! Tile-local integer coordinates are lifted to WGS84 through tippecanoe's
//! 32-bit Web Mercator world coordinate space (`write_json.cpp` /
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
//! duplicated representation: `zoom` (`UInt8`), `layer` (Utf8), and `mvt_id`
//! (`UInt64`, nullable — the raw MVT feature id, if any). They are followed by
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
//!   interior rings before any exterior ring, and rings whose coordinates
//!   are so large their winding cannot be determined exactly) is dropped
//!   rather than emitted; the MVT spec leaves decoder behavior for these
//!   undefined.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::Ordering;
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
use geoarrow_array::capacity::GeometryCapacity;
use geoarrow_array::GeoArrowArray;
use geoparquet::metadata::GeoParquetMetadata;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use prost::Message;
use serde::Serialize;
use thiserror::Error;
use tylertoo_io::KeyValue;

use crate::archive_index::FileBytes;
use crate::compression::{decompress_capped, MAX_TILE_BYTES};
use crate::mvt::{command_decode, zigzag_decode};
use crate::overview::writer::{fold_geo_metadata, geo_metadata_json_deterministic};
use crate::pmtiles_writer::{
    max_expanded_entries, tile_id_to_zxy, walk_directories, ArchiveBytes, DirEntry, DirWalkError,
    Header, TileType, HEADER_BYTES,
};
use crate::vector_tile::tile::GeomType;
use crate::vector_tile::Tile;

/// Rows per output batch (bounds decoder memory to O(batch), matching the
/// export pipeline's streaming ethos).
const BATCH_SIZE: usize = 8192;

/// Provenance / structural column names reserved by the decoder.
const RESERVED_COLUMNS: [&str; 4] = [
    "zoom",
    "layer",
    "mvt_id",
    crate::resolved_schema::DEFAULT_GEOMETRY_COLUMN,
];

// ============================================================================
// Public API
// ============================================================================

/// Options for [`decode_pmtiles`].
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct DecodeOptions {
    /// Only decode tiles with `zoom >= min_zoom`.
    pub min_zoom: Option<u8>,
    /// Only decode tiles with `zoom <= max_zoom`.
    pub max_zoom: Option<u8>,
    /// Only decode features from this MVT layer.
    pub layer: Option<String>,
}

with_setters!(DecodeOptions {
    with_min_zoom => min_zoom: Option<u8>,
    with_max_zoom => max_zoom: Option<u8>,
    with_layer => layer: Option<String>,
});

impl DecodeOptions {
    /// Reject an inverted zoom range (`min_zoom > max_zoom`), which would
    /// otherwise select no tiles and quietly write an empty file.
    /// [`decode_pmtiles`] calls this first; it is public so a caller can
    /// check before doing its own preflight work.
    ///
    /// # Errors
    ///
    /// [`DecodeError::InvalidZoomRange`] when both bounds are set and
    /// `min_zoom > max_zoom`.
    #[doc(hidden)]
    pub fn validate(&self) -> Result<(), DecodeError> {
        match (self.min_zoom, self.max_zoom) {
            (Some(min_zoom), Some(max_zoom)) if min_zoom > max_zoom => {
                Err(DecodeError::InvalidZoomRange { min_zoom, max_zoom })
            }
            _ => Ok(()),
        }
    }
}

/// Summary of a completed decode.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
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
///
/// A variant that wraps an underlying error exposes it through
/// [`std::error::Error::source`] and leaves it out of its own `Display`.
/// The CLI prints the anyhow chain, so writing it in both places showed the
/// cause twice (#396). Callers that want one line use `{:#}` on an
/// `anyhow::Error`, or walk `source()`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DecodeError {
    #[error("minimum zoom {min_zoom} exceeds maximum zoom {max_zoom}")]
    #[non_exhaustive]
    InvalidZoomRange { min_zoom: u8, max_zoom: u8 },

    #[error("I/O error")]
    Io(#[from] std::io::Error),

    /// The output cannot be written (#452: checked before the archive is
    /// read).
    #[error(transparent)]
    Output(#[from] crate::atomic_output::OutputNotWritable),

    #[error(transparent)]
    Archive(#[from] crate::Error),

    #[error("invalid PMTiles archive: {0}")]
    InvalidArchive(String),

    #[error("archive does not contain vector tiles (tile type {0:?})")]
    NotVectorTiles(TileType),

    #[error("failed to decompress {what} at byte offset {offset} ({length} bytes)")]
    #[non_exhaustive]
    Decompress {
        what: &'static str,
        offset: u64,
        length: u64,
        source: std::io::Error,
    },

    #[error("failed to decompress tile z{z}/{x}/{y} at byte offset {offset} ({length} bytes)")]
    #[non_exhaustive]
    TileDecompress {
        z: u8,
        x: u32,
        y: u32,
        offset: u64,
        length: u64,
        source: std::io::Error,
    },

    #[error("MVT protobuf decode failed for tile z{z}/{x}/{y}")]
    #[non_exhaustive]
    Mvt {
        z: u8,
        x: u32,
        y: u32,
        // #437: hidden, so the pre-1.0 `prost` type is not stable API.
        #[doc(hidden)]
        source: prost::DecodeError,
    },

    #[error("invalid MVT geometry in tile z{z}/{x}/{y}: {reason}")]
    #[non_exhaustive]
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

    #[error("Arrow error")]
    // #437: hidden, so the third-party payload type is not stable API.
    #[doc(hidden)]
    Arrow(#[source] arrow_schema::ArrowError),

    #[error("Parquet error")]
    // #437: hidden, so the third-party payload type is not stable API.
    #[doc(hidden)]
    Parquet(#[source] tylertoo_io::ParquetError),

    #[error("GeoParquet write failed: {0}")]
    Write(String),
}

#[doc(hidden)]
impl From<tylertoo_io::ParquetError> for DecodeError {
    fn from(e: tylertoo_io::ParquetError) -> Self {
        DecodeError::Parquet(e)
    }
}

#[doc(hidden)]
impl From<arrow_schema::ArrowError> for DecodeError {
    fn from(e: arrow_schema::ArrowError) -> Self {
        DecodeError::Arrow(e)
    }
}

impl crate::error::Classify for DecodeError {
    /// An inverted zoom range, a raster archive, or a property that collides
    /// with a provenance column is the caller's input problem; a damaged or
    /// unreadable archive, or a failed write, is a failed run.
    fn kind(&self) -> crate::error::ErrorKind {
        use crate::error::ErrorKind;
        match self {
            DecodeError::InvalidZoomRange { .. }
            | DecodeError::NotVectorTiles(_)
            | DecodeError::ReservedColumn(_) => ErrorKind::Input,
            DecodeError::Archive(e) => e.kind(),
            _ => ErrorKind::Runtime,
        }
    }
}

/// Decode a PMTiles archive into a GeoParquet file.
///
/// See the module docs for output semantics (no deduplication, tiled
/// representation, provenance columns, property type unification).
///
/// # Errors
///
/// Returns a [`DecodeError`] if the zoom range is inverted, the archive cannot
/// be read or parsed, does not hold MVT tiles, holds a section or tile that
/// does not decompress or decode, or if writing the output fails.
pub fn decode_pmtiles(
    input_path: impl AsRef<Path>,
    output_path: impl AsRef<Path>,
    options: &DecodeOptions,
) -> Result<DecodeReport, DecodeError> {
    options.validate()?;
    let start = Instant::now();
    // #452: the output is created only after Pass A has decoded every tile.
    crate::atomic_output::check_writable(output_path.as_ref())?;
    let input_path = input_path.as_ref();
    // #462: the header and directories are read up front, and each tile
    // body is read by offset when a pass reaches it (the reader
    // `archive_index` built for the pyramid merge, #498). The input then
    // costs O(directory + one tile), not O(archive).
    // Name the input: the CLI checks it first, a library caller does not.
    let src = FileBytes::open(input_path).map_err(|e| {
        std::io::Error::new(
            e.kind(),
            format!("cannot read {}: {e}", input_path.display()),
        )
    })?;
    let head_len = usize::try_from(src.total_len()).map_or(HEADER_BYTES, |l| l.min(HEADER_BYTES));
    let header = Header::from_bytes(&src.read_range(0, head_len)?)?;
    if !matches!(header.tile_type, TileType::Mvt | TileType::Mlt) {
        return Err(DecodeError::NotVectorTiles(header.tile_type));
    }

    let entries = collect_entries(&src, &header)?;

    // ---- Pass A: property schema union + layer inventory. -----------------
    // Tiles are decoded twice (schema first, rows second) so peak memory
    // stays O(one tile + one batch) instead of O(all features).
    let mut col_types: BTreeMap<String, ColType> = BTreeMap::new();
    let mut layers: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut tiles_read = 0u64;
    for_each_tile(
        &src,
        &header,
        &entries,
        options,
        |features| {
            let mut types: BTreeMap<String, ColType> = BTreeMap::new();
            let mut names: std::collections::BTreeSet<String> = Default::default();
            for (layer_name, feature) in features {
                names.insert(layer_name);
                for (key, value) in &feature.props {
                    if RESERVED_COLUMNS.contains(&key.as_str()) {
                        return Err(DecodeError::ReservedColumn(key.clone()));
                    }
                    let t = ColType::of(value);
                    types
                        .entry(key.clone())
                        .and_modify(|e| *e = e.unify(t))
                        .or_insert(t);
                }
            }
            Ok((names, types))
        },
        |chunk| {
            for (_, (names, types)) in chunk {
                tiles_read += 1;
                layers.extend(names);
                for (key, t) in types {
                    col_types
                        .entry(key)
                        .and_modify(|e| *e = e.unify(t))
                        .or_insert(t);
                }
            }
            Ok(|| Ok(()))
        },
    )?;

    // ---- Output schema: zoom, layer, mvt_id, properties (sorted), geometry.
    let geom_field = GeometryBuilder::new(GeometryType::new(Default::default()))
        .finish()
        .data_type()
        .to_field(crate::resolved_schema::DEFAULT_GEOMETRY_COLUMN, true);
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

    let encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options())
        .map_err(|e| DecodeError::Write(e.to_string()))?;
    // #427: build the output in a sibling and rename it over `output_path`
    // only once it is complete, so an interrupted decode leaves a previous
    // output intact.
    let (file, pending) = crate::atomic_output::create(output_path.as_ref())?;
    let mut writer = tylertoo_io::batch_writer(file, encoder.target_schema())?;

    // ---- Pass B: decode again, cut the rows into BATCH_SIZE batches in tile
    // order, encode the batches on the pool, write them in order. ----------
    let mut report = DecodeReport {
        tiles_read,
        features_written: 0,
        features_skipped: 0,
        layers: layers.into_iter().collect(),
        zoom_range: None,
        elapsed_secs: 0.0,
    };
    let encode = |rows: Vec<Row>| encode_rows(rows, &schema, &col_types);
    // Rows short of a full batch carry over to the next chunk, so batch (and
    // row-group) boundaries are the serial walk's.
    let mut carry: Vec<Row> = Vec::with_capacity(BATCH_SIZE);
    let mut geo_meta = Some(encoder.into_geoparquet_metadata());
    // The writer thread writes one chunk's batches while the pool decodes and
    // encodes the next; the one-slot channel bounds that to two chunks.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<EncodedBatch>>(1);
    let (walked, writer) = std::thread::scope(|s| {
        let geo_meta = &mut geo_meta;
        let write = s.spawn(move || -> Result<_, DecodeError> {
            for batches in rx {
                for (batch, meta) in batches {
                    writer.write(&batch)?;
                    fold_geo_metadata(geo_meta, meta).map_err(DecodeError::Write)?;
                }
            }
            Ok(writer)
        });
        let walked = for_each_tile(&src, &header, &entries, options, Ok, |chunk| {
            let mut full: Vec<Vec<Row>> = Vec::new();
            for (tile, features) in chunk {
                for (layer, feature) in features {
                    let Some(geometry) = feature.geometry else {
                        report.features_skipped += 1;
                        continue;
                    };
                    report.features_written += 1;
                    report.zoom_range = Some(match report.zoom_range {
                        Some((lo, hi)) => (lo.min(tile.z), hi.max(tile.z)),
                        None => (tile.z, tile.z),
                    });
                    carry.push((tile.z, layer, feature.mvt_id, feature.props, geometry));
                    if carry.len() == BATCH_SIZE {
                        full.push(std::mem::replace(
                            &mut carry,
                            Vec::with_capacity(BATCH_SIZE),
                        ));
                    }
                }
            }
            let tx = tx.clone();
            Ok(move || {
                use rayon::prelude::*;
                if !full.is_empty() {
                    let encoded = full.into_par_iter().map(encode).collect::<Result<_, _>>()?;
                    // A send fails only when the writer stopped on its own
                    // error, which the join below returns ahead of this one.
                    tx.send(encoded).map_err(|_| writer_stopped())?;
                }
                Ok(())
            })
        })
        .and_then(|()| {
            if !carry.is_empty() {
                tx.send(vec![encode(std::mem::take(&mut carry))?])
                    .map_err(|_| writer_stopped())?;
            }
            Ok(())
        });
        drop(tx);
        let writer = write
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (walked, writer)
    });
    // A writer error is the cause of the walk's `writer_stopped`.
    let mut writer = writer?;
    walked?;

    // #508: the encoder's own `into_keyvalue()` serializes `geometry_types`
    // straight out of a `HashSet`, so two decodes of the same archive produce
    // footers differing by that array's element order. A decoded archive is
    // the common MIXED-geometry case (points, lines and polygons from one
    // PMTiles archive land in one output file), so take the same
    // deterministic serialization `overview::writer` uses.
    writer.append_key_value_metadata(KeyValue::new(
        "geo".to_string(),
        geo_metadata_json_deterministic(
            geo_meta
                .as_ref()
                .ok_or_else(|| DecodeError::Write("no GeoParquet metadata".into()))?,
        )
        .map_err(|e| DecodeError::Write(e.to_string()))?,
    ));
    writer.close()?;
    #[cfg(test)]
    if FAIL_BEFORE_PUBLISH.with(|f| f.get()) {
        return Err(DecodeError::Write("injected failure before publish".into()));
    }
    pending.publish()?;

    report.elapsed_secs = start.elapsed().as_secs_f64();
    Ok(report)
}

#[cfg(test)]
thread_local! {
    /// Test-only fault (#427): make `decode_pmtiles` fail after the parquet
    /// writer has closed but before the sibling is published, the one point
    /// a real run can only reach through an I/O failure.
    static FAIL_BEFORE_PUBLISH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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

/// `length` bytes at `offset` as an in-file `(start, len)`, or an error
/// naming `what`. Both numbers come from the archive.
fn file_range(
    offset: u64,
    length: u64,
    file_len: u64,
    what: &str,
) -> Result<(usize, usize), DecodeError> {
    let start = usize::try_from(offset)
        .map_err(|_| DecodeError::InvalidArchive(format!("{what} offset overflow")))?;
    let len = usize::try_from(length)
        .map_err(|_| DecodeError::InvalidArchive(format!("{what} length overflow")))?;
    start
        .checked_add(len)
        .filter(|&end| u64::try_from(end).is_ok_and(|end| end <= file_len))
        .ok_or_else(|| DecodeError::InvalidArchive(format!("{what} extends past end of file")))?;
    Ok((start, len))
}

/// Walk the root (and any leaf) directories into one list of entries in
/// ascending tile-id order (the writer emits clustered archives), run
/// lengths left unexpanded, and validate every entry so [`tile_refs`] can
/// expand them blind: its range lies inside the tile-data section and the
/// file, its run fits the archive's expansion budget, and its last tile id
/// is a real one.
///
/// The walk itself is [`walk_directories`], the one every PMTiles reader
/// shares with [`crate::archive_index::ArchiveIndex`] (#779): its section
/// ceilings, leaf bounds and entry and leaf budgets are written once there.
/// Only the per-entry checks below are decode's own, because decode refuses
/// a corrupt archive before it reads a single tile.
///
/// O(directory entries), not O(tiles) (#462): a run of identical ocean
/// tiles is one entry here however many tiles it covers.
fn collect_entries<S: ArchiveBytes + ?Sized>(
    src: &S,
    header: &Header,
) -> Result<Vec<DirEntry>, DecodeError> {
    let entries = walk_directories(src, header).map_err(|e| match e {
        DirWalkError::Decompress {
            what,
            offset,
            length,
            source,
        } => DecodeError::Decompress {
            what,
            offset,
            length,
            source,
        },
        DirWalkError::Read(e) => DecodeError::Archive(e),
        DirWalkError::Invalid(message) => DecodeError::InvalidArchive(message),
    })?;

    // Entry offsets are relative to their section; the base is added here.
    // Both come from the archive, so the sum is checked rather than trusted.
    let absolute = |base: u64, relative: u64, what: &str| -> Result<u64, DecodeError> {
        base.checked_add(relative)
            .ok_or_else(|| DecodeError::InvalidArchive(format!("{what} offset overflow")))
    };

    // An entry's `(offset, length)` must lie inside the tile-data section,
    // not merely inside the file: a tile entry aimed at the directories would
    // otherwise be handed to the MVT decoder (#417).
    let within =
        |offset: u64, length: u64, section_len: u64, what: &str| -> Result<(), DecodeError> {
            match offset.checked_add(length) {
                Some(end) if end <= section_len => Ok(()),
                _ => Err(DecodeError::InvalidArchive(format!(
                    "{what} at {offset} ({length} bytes) extends past its \
                     {section_len}-byte section"
                ))),
            }
        };
    let entry_limit = max_expanded_entries(header);

    // Run lengths are archive-controlled u32s: one entry claiming 0xFFFFFFFF
    // is 137 GB of TileRefs. The budget is what the archive could legitimately
    // address (see `max_expanded_entries`), spent across every entry so a
    // thousand plausible-looking runs cannot add up to the same attack. The
    // runs are expanded lazily by `tile_refs`, but the budget still bounds
    // how long a pass walks.
    let mut budget = entry_limit;
    for entry in &entries {
        within(
            entry.offset,
            u64::from(entry.length),
            header.tile_data_length,
            "tile data",
        )?;
        let run = u64::from(entry.run_length.max(1));
        budget = budget.checked_sub(run).ok_or_else(|| {
            DecodeError::InvalidArchive(format!(
                "directory entry for tile id {} claims a run of {run} tiles, past this \
                 archive's run-length expansion limit of {entry_limit} tiles",
                entry.tile_id,
            ))
        })?;
        // Ids are monotone, so a valid last id vouches for the whole run.
        let last = entry.tile_id.checked_add(run - 1).ok_or_else(|| {
            DecodeError::InvalidArchive(format!(
                "run of {run} tiles from tile id {} overflows the tile-id space",
                entry.tile_id
            ))
        })?;
        tile_id_to_zxy(last)?;
        let start = absolute(header.tile_data_offset, entry.offset, "tile data")?;
        file_range(start, u64::from(entry.length), src.total_len(), "tile data")?;
    }
    Ok(entries)
}

/// Every tile `entries` address, run lengths expanded and the zoom filter
/// applied, in ascending tile-id order. Lazy, so a pass holds one
/// [`TileRef`] at a time rather than all of them (#462).
///
/// `entries` come from [`collect_entries`], which validated every range and
/// id; the conversions here stay checked so a broken invariant is an error,
/// not a panic or a wrapped offset.
fn tile_refs<'a>(
    entries: &'a [DirEntry],
    header: &'a Header,
    options: &'a DecodeOptions,
) -> impl Iterator<Item = Result<TileRef, DecodeError>> + 'a {
    entries.iter().flat_map(move |entry| {
        (0..u64::from(entry.run_length.max(1)))
            .filter_map(move |i| tile_ref(entry, i, header, options).transpose())
    })
}

/// The `i`th tile of `entry`'s run, or `None` when the zoom filter drops it.
fn tile_ref(
    entry: &DirEntry,
    i: u64,
    header: &Header,
    options: &DecodeOptions,
) -> Result<Option<TileRef>, DecodeError> {
    let tile_id = entry
        .tile_id
        .checked_add(i)
        .ok_or_else(|| DecodeError::InvalidArchive("tile id overflow".to_string()))?;
    let (z, x, y) = tile_id_to_zxy(tile_id)?;
    if options.min_zoom.is_some_and(|mz| z < mz) || options.max_zoom.is_some_and(|mz| z > mz) {
        return Ok(None);
    }
    let start = header
        .tile_data_offset
        .checked_add(entry.offset)
        .and_then(|s| usize::try_from(s).ok())
        .ok_or_else(|| DecodeError::InvalidArchive("tile data offset overflow".to_string()))?;
    Ok(Some(TileRef {
        z,
        x,
        y,
        start,
        len: entry.length as usize,
    }))
}

/// Compressed tile bytes per parallel chunk. The chunk's decoded features
/// (gzip'd MVT inflated roughly 50x into `geo` geometries and owned
/// properties) and the batches cut from them are what parallel decoding
/// holds beyond the serial walk, so this bounds that in bytes. A tile larger
/// than this is a chunk of its own. 2 MiB gives 16 threads several batches
/// to encode per chunk; see [`DECODE_THREADS`] for the curve.
const CHUNK_BYTES: usize = 2 << 20;

/// Most threads that decode and encode. Each thread keeps its allocator
/// high-water for the rest of the run, so peak RSS grows with the threads
/// used, not the work (#817); 16 is where more threads stop buying wall.
const DECODE_THREADS: usize = 16;

/// A pool of at most [`DECODE_THREADS`] workers for decoding tile bodies, or
/// `None` when the current pool is already that small. `stats --layers`
/// decodes on it too, so a crafted archive cannot inflate one 64 MiB body
/// per core at once.
pub(crate) fn decode_pool() -> Result<Option<rayon::ThreadPool>, rayon::ThreadPoolBuildError> {
    let threads = rayon::current_num_threads().min(DECODE_THREADS);
    (threads < rayon::current_num_threads())
        .then(|| rayon::ThreadPoolBuilder::new().num_threads(threads).build())
        .transpose()
}

/// Most tiles per chunk, whatever their bytes: a run of empty tile bodies
/// would otherwise make one chunk of millions of tiles.
const CHUNK_TILES: usize = 4096;

/// Most decompressed tile bytes a chunk keeps decoded before [`for_each_tile`]
/// hands them to its sink. A 2 MiB chunk of ordinary tiles inflates to tens
/// of MiB; a crafted archive of highly compressible tiles (each capped at
/// [`MAX_TILE_BYTES`] plain) could otherwise hold thousands of them decoded
/// at once, where the serial walk held one.
const CHUNK_DECODED_BYTES: usize = 256 << 20;

#[cfg(test)]
thread_local! {
    /// Test override of [`CHUNK_DECODED_BYTES`].
    static CHUNK_DECODED_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    /// Chunks [`for_each_tile`] dispatched; a structural counter for tests.
    static CHUNKS_DISPATCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Test override of [`CHUNK_BYTES`].
    static CHUNK_BYTES_OVERRIDE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Decode every selected tile in chunks of about [`CHUNK_BYTES`] compressed
/// bytes. One [`TileReader`] reads the bodies in tile order (#462: one
/// positioned read per window), the pool decompresses and decodes them and
/// runs `work` on each tile's features, and `sink` gets each chunk's results
/// in tile order. The job `sink` returns runs on the pool beside the next
/// chunk's decode. The output therefore does not depend on the thread count,
/// and the first error in tile order wins.
fn for_each_tile<S, T, W, K, J>(
    src: &S,
    header: &Header,
    entries: &[DirEntry],
    options: &DecodeOptions,
    work: W,
    mut sink: K,
) -> Result<(), DecodeError>
where
    S: ArchiveBytes + ?Sized,
    T: Send,
    W: Fn(Vec<(String, DecodedFeature)>) -> Result<T, DecodeError> + Sync,
    K: FnMut(Vec<(TileRef, T)>) -> Result<J, DecodeError>,
    J: FnOnce() -> Result<(), DecodeError> + Send,
{
    use rayon::prelude::*;
    let mut reader = TileReader::new(src);
    let mut refs = tile_refs(entries, header, options);
    let chunk_bytes = CHUNK_BYTES;
    #[cfg(test)]
    let chunk_bytes = CHUNK_BYTES_OVERRIDE
        .with(|c| c.get())
        .unwrap_or(chunk_bytes);
    let decoded_cap = CHUNK_DECODED_BYTES;
    #[cfg(test)]
    let decoded_cap = CHUNK_DECODED_OVERRIDE
        .with(|c| c.get())
        .unwrap_or(decoded_cap);
    let pool = decode_pool().map_err(|e| DecodeError::Write(format!("decode thread pool: {e}")))?;
    let mut job: Option<J> = None;
    let run = |job: Option<J>| job.map_or(Ok(()), |job| job());
    // Every job runs on the capped pool, the last one included.
    let in_pool = |job: Option<J>| match &pool {
        Some(pool) => pool.install(|| run(job)),
        None => run(job),
    };
    // An unreadable tile ends the walk after the tiles before it, which
    // may hold an earlier error.
    let mut stop = None;
    // A chunk's tiles from the first one left undecoded past `decoded_cap`:
    // they start the next chunk, those already decoded with their result
    // and decompressed size.
    #[expect(
        clippy::type_complexity,
        reason = "one slot per tile; a named type would need the walk's generics"
    )]
    let mut carry: Vec<(TileRef, Vec<u8>, Option<(Result<T, DecodeError>, usize)>)> = Vec::new();
    loop {
        let mut chunk = std::mem::take(&mut carry);
        let mut bytes: usize = chunk.iter().map(|(_, body, _)| body.len()).sum();
        // Empty bodies weigh nothing, so the tile count is capped too.
        while stop.is_none() && bytes < chunk_bytes && chunk.len() < CHUNK_TILES {
            let body = match refs.next() {
                None => break,
                Some(tile) => tile.and_then(|tile| {
                    let body = read_tile_body(&mut reader, &tile)?.into_owned();
                    Ok((tile, body))
                }),
            };
            match body {
                Ok((tile, body)) => {
                    bytes += body.len();
                    chunk.push((tile, body, None));
                }
                Err(e) => {
                    stop = Some(e);
                    break;
                }
            }
        }
        if chunk.is_empty() {
            in_pool(job)?;
            return stop.map_or(Ok(()), Err);
        }
        #[cfg(test)]
        CHUNKS_DISPATCHED.with(|c| c.set(c.get() + 1));
        let prev = job.take();
        #[expect(
            clippy::disallowed_types,
            reason = "#817: a per-chunk count shared with its decode tasks"
        )]
        let decoded = std::sync::atomic::AtomicUsize::new(
            chunk
                .iter()
                .filter_map(|(_, _, slot)| slot.as_ref().map(|(_, plain)| plain))
                .sum(),
        );
        let step = || {
            rayon::join(
                || run(prev),
                || {
                    chunk
                        .par_iter_mut()
                        .enumerate()
                        .for_each(|(i, (tile, body, slot))| {
                            // Past the cap, a tile waits for the next chunk;
                            // the first always decodes, so the walk advances.
                            if slot.is_some()
                                || (i > 0 && decoded.load(Ordering::Relaxed) >= decoded_cap)
                            {
                                return;
                            }
                            *slot = Some(match decode_tile_features(body, header, tile, options) {
                                Ok((features, plain)) => {
                                    decoded.fetch_add(plain, Ordering::Relaxed);
                                    (work(features), plain)
                                }
                                Err(e) => (Err(e), 0),
                            });
                        });
                },
            )
        };
        let (prev, ()) = match &pool {
            Some(pool) => pool.install(step),
            None => step(),
        };
        prev?;
        let taken = chunk
            .iter()
            .take_while(|(_, _, slot)| slot.is_some())
            .count();
        carry = chunk.split_off(taken);
        let done = chunk
            .into_iter()
            .filter_map(|(tile, _, slot)| slot.map(|(r, _)| r.map(|t| (tile, t))))
            .collect::<Result<Vec<_>, DecodeError>>()?;
        job = Some(sink(done)?);
        if carry.is_empty() {
            if let Some(e) = stop {
                in_pool(job)?;
                return Err(e);
            }
        }
    }
}

/// How much tile data [`TileReader`] reads at a time.
const TILE_READ_WINDOW: usize = 1 << 20;

/// Read-ahead for tile bodies (#462).
///
/// A clustered archive stores tile bodies in the order a pass visits them,
/// so reading a window at a time costs one positioned read per
/// [`TILE_READ_WINDOW`] instead of one per tile, and a run of tiles sharing
/// one body is served from the window too. With one read per tile, a full
/// decode of a 100 MB, 147k-tile archive ran ~3% slower than reading the
/// whole file. A body behind the window (a deduplicated tile's
/// back-reference) or larger than it is read on its own. Memory stays
/// O(window + one tile).
struct TileReader<'a, S: ?Sized> {
    src: &'a S,
    window: Vec<u8>,
    /// File offset of `window[0]`.
    at: usize,
}

impl<'a, S: ArchiveBytes + ?Sized> TileReader<'a, S> {
    fn new(src: &'a S) -> Self {
        TileReader {
            src,
            window: Vec::new(),
            at: 0,
        }
    }

    /// `len` bytes at `start`. The caller has checked that the range lies
    /// inside the file ([`collect_entries`]).
    fn read(
        &mut self,
        start: usize,
        len: usize,
    ) -> Result<std::borrow::Cow<'_, [u8]>, DecodeError> {
        let end = start + len;
        if start < self.at || end > self.at + self.window.len() {
            // Behind the window is a deduplicated tile pointing back at an
            // earlier body; refilling for it would drag the window backwards
            // and cost a full window per back-reference.
            if start < self.at || len > TILE_READ_WINDOW {
                return Ok(self.src.read_range(start, len)?);
            }
            let file_len = usize::try_from(self.src.total_len()).unwrap_or(usize::MAX);
            // `end <= file_len`, so the window always covers the tile.
            let window = TILE_READ_WINDOW.min(file_len - start);
            self.window = self.src.read_range(start, window)?.into_owned();
            self.at = start;
        }
        Ok(std::borrow::Cow::Borrowed(
            &self.window[start - self.at..end - self.at],
        ))
    }
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

/// `tile`'s compressed body. The body is read by offset (#462), which
/// allocates whatever length the directory claims, so the length is capped
/// before the read (#510). A compressed body larger than the ceiling it
/// decompresses under cannot be a legitimate tile.
fn read_tile_body<'r, S: ArchiveBytes + ?Sized>(
    reader: &'r mut TileReader<'_, S>,
    tile: &TileRef,
) -> Result<std::borrow::Cow<'r, [u8]>, DecodeError> {
    if tile.len as u64 > MAX_TILE_BYTES {
        return Err(tile.decompress_error(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "compressed body of {} bytes exceeds the {MAX_TILE_BYTES}-byte tile ceiling",
                tile.len
            ),
        )));
    }
    reader.read(tile.start, tile.len)
}

impl TileRef {
    fn decompress_error(&self, source: std::io::Error) -> DecodeError {
        DecodeError::TileDecompress {
            z: self.z,
            x: self.x,
            y: self.y,
            offset: self.start as u64,
            length: self.len as u64,
            source,
        }
    }
}

/// Decompress and decode one tile's compressed `raw` body into
/// `(layer_name, feature)` pairs, applying the layer filter and the
/// coordinate transform.
fn decode_tile_features(
    raw: &[u8],
    header: &Header,
    tile: &TileRef,
    options: &DecodeOptions,
) -> Result<(Vec<(String, DecodedFeature)>, usize), DecodeError> {
    let mut plain = decompress_capped(raw, header.tile_compression, MAX_TILE_BYTES)
        .map_err(|e| tile.decompress_error(e))?;
    if header.tile_type == TileType::Mlt {
        plain = crate::tile_format::mlt_to_mvt(&plain).map_err(|e| {
            DecodeError::InvalidArchive(format!(
                "tile z{}/{}/{} is not a valid MLT tile: {e}",
                tile.z, tile.x, tile.y
            ))
        })?;
    }
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
        // #433: `tile_local_to_lonlat` divides the tile width by the extent,
        // so a layer declaring 0 would turn every coordinate into ±inf (NaN
        // at the origin) and write that straight into the parquet. The MVT
        // spec leaves the field's range implicit; zero is never a resolution.
        if extent == 0 {
            return Err(DecodeError::InvalidArchive(format!(
                "tile z{}/{}/{} layer {:?} declares extent 0; a layer's extent is its \
                 tile-local coordinate resolution and must be positive",
                tile.z, tile.x, tile.y, layer.name
            )));
        }
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
    Ok((out, plain.len()))
}

/// Fuzz hook (#424): decode one *uncompressed* MVT tile at `(z, x, y)`
/// exactly as [`decode_pmtiles`] would, and return
/// `(features decoded, features whose geometry decoded to something)`.
///
/// This is the tile-bytes surface of the decoder — protobuf parse, command
/// stream, ring assembly, tag lookup — reached with the archive framing
/// (directory walk, decompression) stripped away so libFuzzer spends its
/// budget on the parser rather than on gzip. Decompression is fuzzed on its
/// own through the `decompress` target, and the whole chain through
/// [`fuzz_decode_archive`]. Only the `fuzz/` crate calls this; the `fuzzing`
/// feature is never on in a normal build.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_decode_tile(
    z: u8,
    x: u32,
    y: u32,
    plain: &[u8],
) -> Result<(usize, usize), DecodeError> {
    let header = Header {
        tile_compression: crate::compression::Compression::None,
        ..Header::default()
    };
    let tile = TileRef {
        z,
        x,
        y,
        start: 0,
        len: plain.len(),
    };
    let (decoded, _) = decode_tile_features(plain, &header, &tile, &DecodeOptions::default())?;
    let with_geometry = decoded.iter().filter(|(_, f)| f.geometry.is_some()).count();
    Ok((decoded.len(), with_geometry))
}

/// Fuzz hook (#633): [`decode_pmtiles`] on an in-memory archive, minus the
/// GeoParquet writer.
///
/// Runs everything `decode` does to an archive it reads: the header, the
/// tile-type check, the directory walk with its leaf/entry/run-length budgets
/// ([`collect_entries`]), and for every addressed tile the capped
/// decompression and MVT decode, the reserved-column check and the property
/// type union. [`fuzz_decode_tile`] reaches only the last step. Returns
/// `(tiles, features)`. Only the `fuzz/` crate calls this; the `fuzzing`
/// feature is never on in a normal build.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub fn fuzz_decode_archive(bytes: &[u8]) -> Result<(usize, usize), DecodeError> {
    let options = DecodeOptions::default();
    let header = Header::from_bytes(bytes)?;
    if !matches!(header.tile_type, TileType::Mvt | TileType::Mlt) {
        return Err(DecodeError::NotVectorTiles(header.tile_type));
    }
    let entries = collect_entries(&bytes, &header)?;
    let mut col_types: BTreeMap<String, ColType> = BTreeMap::new();
    let mut features = 0;
    let mut tiles = 0;
    let mut reader = TileReader::new(&bytes);
    for tile in tile_refs(&entries, &header, &options) {
        let tile = tile?;
        tiles += 1;
        let body = read_tile_body(&mut reader, &tile)?;
        for (_layer, feature) in decode_tile_features(&body, &header, &tile, &options)?.0 {
            features += 1;
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
    Ok((tiles, features))
}

// ============================================================================
// MVT geometry command stream -> parts -> geo::Geometry
// ============================================================================

/// One MoveTo-initiated run of vertices in tile-local coordinates.
struct Part {
    /// Vertices, without the implicit `ClosePath` closing vertex.
    pts: Vec<(i64, i64)>,
    /// Whether the part was terminated by `ClosePath` (i.e. it is a ring).
    closed: bool,
}

impl Part {
    /// Twice the signed area by the surveyor's formula on raw tile coords.
    /// Per the MVT spec, positive ⇒ exterior ring, negative ⇒ interior ring
    /// (Y axis points down in tile space).
    ///
    /// Computed in `i128`, not `i64`: tile coordinates are accumulated
    /// zigzag deltas, so a hostile tile can walk them past 2^32 and make the
    /// cross products overflow `i64` — which panics in debug and, in
    /// release, *flips the sign* and with it the exterior/interior
    /// classification (the decode-side twin of #406). `i128` is branch-free
    /// for every product (both factors fit `i64`, so the product fits), but
    /// it is not unconditionally enough: the running sum of a ring with
    /// millions of vertices could still overflow, so the additions are
    /// checked. `None` means the ring is so large its winding cannot be
    /// determined exactly, and the caller drops it like a zero-area ring.
    fn area2(&self) -> Option<i128> {
        let n = self.pts.len();
        let mut sum: i128 = 0;
        for i in 0..n {
            let (x0, y0) = self.pts[i];
            let (x1, y1) = self.pts[(i + 1) % n];
            let cross =
                (i128::from(x0) * i128::from(y1)).checked_sub(i128::from(x1) * i128::from(y0))?;
            sum = sum.checked_add(cross)?;
        }
        Some(sum)
    }
}

/// Parse an MVT geometry command stream (MoveTo/LineTo/ClosePath with
/// zigzag-encoded deltas) into parts. Each `MoveTo` coordinate starts a new
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
                // `None` is an unclassifiable ring (see `area2`), dropped
                // exactly like the zero-area case below.
                let Some(area2) = part.area2() else {
                    continue;
                };
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

/// The GeoParquet encoding options of every decode encoder. Built per
/// encoder: the options hold a CRS transform that is not `Sync`.
fn gpq_options() -> geoparquet::writer::GeoParquetWriterOptions {
    GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
        .build()
}

/// One output row: zoom, layer, MVT id, properties, geometry.
type Row = (
    u8,
    String,
    Option<u64>,
    Vec<(String, PropValue)>,
    Geometry<f64>,
);

fn writer_stopped() -> DecodeError {
    DecodeError::Write("the GeoParquet writer stopped".into())
}

/// An encoded batch and the GeoParquet metadata its encoder gathered.
type EncodedBatch = (RecordBatch, GeoParquetMetadata);

/// Build and encode one output batch. Each batch has its own encoder, so
/// batches encode in parallel; [`fold_geo_metadata`] folds their metadata
/// in batch order into what one encoder fed every batch would report.
fn encode_rows(
    rows: Vec<Row>,
    schema: &Arc<Schema>,
    col_types: &BTreeMap<String, ColType>,
) -> Result<EncodedBatch, DecodeError> {
    let mut batch = RowBatch::new(col_types);
    for (zoom, layer, mvt_id, props, geometry) in rows {
        batch.push(zoom, &layer, mvt_id, &props, geometry);
    }
    // #828: sized up front, so the coordinate buffers never grow by doubling.
    let capacity = GeometryCapacity::from_geometries(batch.geoms.iter().map(Some))
        .map_err(|e| DecodeError::Write(e.to_string()))?;
    let mut geom_builder =
        GeometryBuilder::with_capacity(GeometryType::new(Default::default()), capacity);
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
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(schema, &gpq_options())
        .map_err(|e| DecodeError::Write(e.to_string()))?;
    let encoded = encoder
        .encode_record_batch(&record)
        .map_err(|e| DecodeError::Write(e.to_string()))?;
    Ok((encoded, encoder.into_geoparquet_metadata()))
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mvt::{command_encode, encode_geometry, zigzag_encode};
    use crate::tile::MAX_LATITUDE;
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
        assert!((lat - MAX_LATITUDE).abs() < 1e-6); // top of Mercator
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
        let (commands, geom_type) = encode_geometry(
            geom,
            &bounds,
            4096,
            &crate::diagnostics::Diagnostics::default(),
        );
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
    fn ring_area_survives_saturated_coordinate_deltas() {
        // A hostile tile can walk the cursor to ~2^32 with two maximal
        // zigzag deltas per axis. The doubled area of the resulting ring is
        // ~1.8e19, past i64::MAX: computed in i64 it panics in debug and
        // wraps *negative* in release, turning an exterior ring into an
        // interior one (the decode-side twin of #406).
        let big = i32::MAX;
        let geom = [
            command_encode(1, 1),
            zigzag_encode(0),
            zigzag_encode(0),
            command_encode(2, 4),
            zigzag_encode(big),
            zigzag_encode(0),
            zigzag_encode(big),
            zigzag_encode(0),
            zigzag_encode(0),
            zigzag_encode(big),
            zigzag_encode(0),
            zigzag_encode(big),
            command_encode(7, 1),
        ];
        let parts = parse_command_stream(&geom).unwrap();
        assert_eq!(parts[0].pts.len(), 5, "ring must have been parsed whole");
        assert!(
            matches!(
                assemble_geometry(GeomType::Polygon, &parts, |x, y| (x as f64, y as f64)),
                Some(Geometry::Polygon(_))
            ),
            "a positively wound ring must stay an exterior ring"
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
        assert!(
            parts[0].area2().unwrap() < 0,
            "test ring must be interior-wound"
        );
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

    /// #462: whatever order and size the reads come in (forward, backward,
    /// straddling a window edge, larger than the window, ending exactly at
    /// end of file), `TileReader` hands back the same bytes as slicing the
    /// whole archive did.
    #[test]
    fn tile_reader_matches_direct_slices() {
        let file: Vec<u8> = (0..3 * TILE_READ_WINDOW + 17)
            .map(|i| (i * 31 % 251) as u8)
            .collect();
        let src: &[u8] = &file;
        let mut reader = TileReader::new(&src);
        let w = TILE_READ_WINDOW;
        let reads = [
            (0, 10),
            (10, 100),
            (10, 100),      // a run sharing one body
            (w - 5, 10),    // straddles the first window's edge
            (3, 7),         // backwards, out of the current window
            (w + 1, 2 * w), // larger than a window
            (file.len() - 1, 1),
            (file.len() - w, w), // a full window ending at end of file
            (5, 0),
        ];
        for (start, len) in reads {
            let got = reader.read(start, len).unwrap();
            assert_eq!(&got[..], &file[start..start + len], "read({start}, {len})");
        }
    }

    /// An in-memory source that counts the positioned reads made of it.
    struct CountingBytes<'a> {
        bytes: &'a [u8],
        reads: std::cell::Cell<usize>,
    }

    impl ArchiveBytes for CountingBytes<'_> {
        fn total_len(&self) -> u64 {
            self.bytes.len() as u64
        }

        fn read_range(
            &self,
            offset: usize,
            len: usize,
        ) -> crate::Result<std::borrow::Cow<'_, [u8]>> {
            self.reads.set(self.reads.get() + 1);
            Ok(std::borrow::Cow::Borrowed(
                &self.bytes[offset..offset + len],
            ))
        }
    }

    /// The window is what makes ranged decode as fast as the whole-file read
    /// (#462), so which reads it serves is pinned, not just their bytes:
    /// every read inside the window, edges included, costs no source read; a
    /// back-reference or an over-window body is read on its own and leaves
    /// the window where it was.
    #[test]
    fn tile_reader_serves_the_window_without_rereading() {
        let w = TILE_READ_WINDOW;
        let file: Vec<u8> = (0..3 * w + 17).map(|i| (i * 31 % 251) as u8).collect();
        let src = CountingBytes {
            bytes: &file,
            reads: std::cell::Cell::new(0),
        };
        let mut reader = TileReader::new(&src);
        // (start, len, source reads so far)
        let reads = [
            (0, 10, 1),      // first read fills [0, w)
            (5, 5, 1),       // inside it
            (0, 10, 1),      // starting on its first byte
            (w - 10, 10, 1), // ending on its last byte
            (w, 10, 2),      // past it: refill at w
            (3, 7, 3),       // behind it: read directly
            (w + 5, 5, 3),   // the window stayed at w
            (0, w + 1, 4),   // larger than a window: read directly
            (w + 9, 1, 4),   // the window stayed at w
            (2 * w, w, 5),   // exactly a window: refill at 2w
            (2 * w, w, 5),   // the same body again, a run sharing it
        ];
        for (start, len, want) in reads {
            let got = reader.read(start, len).unwrap();
            assert_eq!(&got[..], &file[start..start + len], "read({start}, {len})");
            assert_eq!(
                src.reads.get(),
                want,
                "source reads after read({start}, {len})"
            );
        }
    }

    /// roborev 344: the parallel walk keeps #462's read-ahead. One reader
    /// walks the tiles in order however the chunks and threads fall, so 40
    /// small clustered tiles cost one positioned read, not one window each.
    #[test]
    fn parallel_walk_reads_each_window_once() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::read(small_archive_of(dir.path(), 40)).unwrap();
        let header = Header::from_bytes(&file[..HEADER_BYTES]).unwrap();
        let entries = collect_entries(&file.as_slice(), &header).unwrap();
        let src = CountingBytes {
            bytes: &file,
            reads: std::cell::Cell::new(0),
        };
        let tiles = {
            CHUNK_BYTES_OVERRIDE.with(|c| c.set(Some(1)));
            let mut tiles = 0;
            for_each_tile(
                &src,
                &header,
                &entries,
                &DecodeOptions::default(),
                |_| Ok(()),
                |chunk| {
                    tiles += chunk.len();
                    Ok(|| Ok(()))
                },
            )
            .unwrap();
            CHUNK_BYTES_OVERRIDE.with(|c| c.set(None));
            tiles
        };
        assert_eq!((tiles, src.reads.get()), (40, 1));
    }

    /// #817 review: a chunk closes on its decoded bytes too, so tiles that
    /// inflate far beyond their compressed size are not all held decoded at
    /// once. Past the cap the rest of the chunk waits for the next one; the
    /// sink sees the same tiles in the same order.
    #[test]
    fn a_chunk_closes_on_its_decoded_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let file = std::fs::read(small_archive_of(dir.path(), 40)).unwrap();
        let header = Header::from_bytes(&file[..HEADER_BYTES]).unwrap();
        let entries = collect_entries(&file.as_slice(), &header).unwrap();
        let walk = |cap: Option<usize>| {
            // One thread, so tiles decode in order and the split is exact.
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .unwrap();
            pool.install(|| {
                CHUNK_DECODED_OVERRIDE.with(|c| c.set(cap));
                let mut chunks = Vec::new();
                for_each_tile(
                    &file.as_slice(),
                    &header,
                    &entries,
                    &DecodeOptions::default(),
                    |features| Ok(features.len()),
                    |chunk| {
                        chunks.push(
                            chunk
                                .into_iter()
                                .map(|(t, n)| (t.z, t.x, t.y, n))
                                .collect::<Vec<_>>(),
                        );
                        Ok(|| Ok(()))
                    },
                )
                .unwrap();
                CHUNK_DECODED_OVERRIDE.with(|c| c.set(None));
                chunks
            })
        };
        let whole = walk(None);
        let split = walk(Some(1));
        // On several threads a later tile can finish before an earlier one
        // stops at the cap: it is carried decoded, never decoded again.
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(4)
            .build()
            .unwrap();
        for _ in 0..20 {
            let calls = std::sync::Mutex::new(0usize);
            let tiles = pool.install(|| {
                CHUNK_DECODED_OVERRIDE.with(|c| c.set(Some(1)));
                let mut tiles = Vec::new();
                for_each_tile(
                    &file.as_slice(),
                    &header,
                    &entries,
                    &DecodeOptions::default(),
                    |features| {
                        *calls.lock().unwrap() += 1;
                        Ok(features.len())
                    },
                    |chunk| {
                        tiles.extend(chunk.into_iter().map(|(t, n)| (t.z, t.x, t.y, n)));
                        Ok(|| Ok(()))
                    },
                )
                .unwrap();
                CHUNK_DECODED_OVERRIDE.with(|c| c.set(None));
                tiles
            });
            assert_eq!(tiles, whole.concat());
            assert_eq!(
                calls.into_inner().unwrap(),
                tiles.len(),
                "a tile decoded twice"
            );
        }
        assert_eq!(whole.len(), 1, "the fixture fits one chunk by bytes");
        assert!(
            split.iter().all(|c| c.len() == 1),
            "{:?}",
            split.iter().map(Vec::len).collect::<Vec<_>>()
        );
        assert_eq!(split.concat(), whole.concat());
    }

    /// roborev 368: a run of empty tile bodies weighs no bytes, so the tile
    /// count caps a chunk too; one entry for 10,000 empty tiles must not
    /// become one 10,000-tile chunk.
    #[test]
    fn empty_bodies_still_split_into_bounded_chunks() {
        let header = Header {
            tile_compression: crate::compression::Compression::None,
            ..Header::default()
        };
        let entries = [DirEntry {
            tile_id: 0,
            offset: 0,
            length: 0,
            run_length: 10_000,
        }];
        let (mut tiles, mut largest) = (0, 0);
        for_each_tile(
            &[0u8; 16].as_slice(),
            &header,
            &entries,
            &DecodeOptions::default(),
            |_| Ok(()),
            |chunk| {
                tiles += chunk.len();
                largest = largest.max(chunk.len());
                Ok(|| Ok(()))
            },
        )
        .unwrap();
        assert_eq!(tiles, 10_000);
        assert!(largest <= CHUNK_TILES, "{largest}-tile chunk");
    }

    /// The batch metadata fold must report what main's single serial encoder
    /// did, including for a batch whose geometries hold no coordinates (an
    /// empty MultiPoint here): geoparquet's `BoundingRect::add_rect` adds
    /// that batch's empty rect corner by corner.
    #[test]
    fn folded_batch_metadata_matches_one_serial_encoder() {
        let schema = Arc::new(Schema::new(vec![GeometryBuilder::new(GeometryType::new(
            Default::default(),
        ))
        .finish()
        .data_type()
        .to_field(crate::resolved_schema::DEFAULT_GEOMETRY_COLUMN, true)]));
        let batch = |geoms: Vec<Geometry<f64>>| {
            let mut b = GeometryBuilder::new(GeometryType::new(Default::default()));
            b.extend_from_iter(geoms.iter().map(Some));
            RecordBatch::try_new(schema.clone(), vec![b.finish().to_array_ref()]).unwrap()
        };
        let batches = [
            batch(vec![Geometry::Point(Point::new(1.0, 2.0))]),
            batch(vec![Geometry::MultiPoint(MultiPoint::new(vec![]))]),
            batch(vec![Geometry::Point(Point::new(-3.0, 0.5))]),
        ];
        let bbox = |m: &GeoParquetMetadata| {
            m.columns[crate::resolved_schema::DEFAULT_GEOMETRY_COLUMN]
                .bbox
                .clone()
        };
        let mut serial = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options()).unwrap();
        let mut folded = None;
        for b in &batches {
            serial.encode_record_batch(b).unwrap();
            let mut own = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options()).unwrap();
            own.encode_record_batch(b).unwrap();
            fold_geo_metadata(&mut folded, own.into_geoparquet_metadata()).unwrap();
        }
        assert_eq!(
            bbox(&folded.unwrap()),
            bbox(&serial.into_geoparquet_metadata())
        );
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

    // ---- #427: the destination is untouched until the decode completes. --

    /// A minimal MVT tile: one layer `t` with one point feature at (1, 1).
    const POINT_TILE: &[u8] = &[
        0x1A, 0x11, // Tile.layers, 17 bytes
        0x0A, 0x01, b't', // name
        0x12, 0x07, 0x18, 0x01, 0x22, 0x03, 0x09, 0x02, 0x02, // feature: POINT, MoveTo(1,1)
        0x28, 0x80, 0x20, // extent 4096
        0x78, 0x02, // version 2
    ];

    fn small_archive(dir: &Path) -> std::path::PathBuf {
        small_archive_of(dir, 4)
    }

    fn small_archive_of(dir: &Path, tiles: u32) -> std::path::PathBuf {
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        let path = dir.join("in.pmtiles");
        let mut writer = StreamingPmtilesWriter::new(crate::Compression::Gzip).unwrap();
        writer.set_layer_name("t");
        for i in 0..tiles {
            let mut tile = POINT_TILE.to_vec();
            tile[12] = 0x02 + 2 * (i as u8); // MoveTo x = 1 + i (zigzag)
            writer.add_tile(5, i % 32, 7 + i / 32, &tile).unwrap();
        }
        writer.finalize(&path).unwrap();
        path
    }

    fn dir_listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// A decode that fails after the parquet writer has already produced
    /// row groups (the fault is injected between the writer's close and the
    /// publish — the only post-open failure the production path can hit is
    /// I/O, which is not provokable on demand) must leave the previous
    /// output intact and no sibling behind (#427).
    #[test]
    fn interrupted_decode_leaves_previous_output_intact() {
        let dir = tempfile::tempdir().unwrap();
        let input = small_archive(dir.path());
        let out = dir.path().join("decoded.parquet");
        std::fs::write(&out, b"previous good decode").unwrap();

        let err = FAIL_BEFORE_PUBLISH.with(|f| {
            f.set(true);
            let r = decode_pmtiles(&input, &out, &DecodeOptions::default());
            f.set(false);
            r.unwrap_err()
        });
        assert!(matches!(err, DecodeError::Write(_)), "{err}");

        assert_eq!(std::fs::read(&out).unwrap(), b"previous good decode");
        assert_eq!(
            dir_listing(dir.path()),
            vec!["decoded.parquet".to_string(), "in.pmtiles".to_string()],
            "no sibling left beside the output"
        );
    }

    /// The successful path replaces the previous output wholesale, by
    /// rename, and leaves nothing else in the directory.
    #[test]
    fn decode_publishes_over_previous_output_without_litter() {
        let dir = tempfile::tempdir().unwrap();
        let input = small_archive(dir.path());
        let out = dir.path().join("decoded.parquet");
        std::fs::write(&out, b"previous good decode").unwrap();

        let report = decode_pmtiles(&input, &out, &DecodeOptions::default()).unwrap();
        assert_eq!(report.features_written, 4);
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(&bytes[..4], b"PAR1");
        assert_eq!(
            dir_listing(dir.path()),
            vec!["decoded.parquet".to_string(), "in.pmtiles".to_string()]
        );
    }

    /// Output is byte-identical at any thread count, and the tiles are
    /// dispatched in chunks bounded by compressed bytes, not by tile or
    /// thread count: a one-byte cap makes every tile its own chunk, the
    /// default cap fits all 40 small tiles in one, on 1 and 4 threads alike.
    #[test]
    fn decode_is_thread_count_independent_and_chunked_by_bytes() {
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.pmtiles");
        let mut writer = StreamingPmtilesWriter::new(crate::Compression::Gzip).unwrap();
        writer.set_layer_name("t");
        for i in 0..40u32 {
            let mut tile = POINT_TILE.to_vec();
            tile[12] = 0x02 + 2 * (i as u8 % 8);
            writer.add_tile(6, i, 7, &tile).unwrap();
        }
        writer.finalize(&input).unwrap();

        let run = |threads: usize, cap: Option<usize>| {
            let out = dir.path().join(format!("out{threads}.parquet"));
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let chunks = pool.install(|| {
                CHUNKS_DISPATCHED.with(|c| c.set(0));
                CHUNK_BYTES_OVERRIDE.with(|c| c.set(cap));
                decode_pmtiles(&input, &out, &DecodeOptions::default()).unwrap();
                CHUNKS_DISPATCHED.with(|c| c.get())
            });
            (std::fs::read(&out).unwrap(), chunks)
        };
        let (one, c1) = run(1, None);
        let (four, c4) = run(4, None);
        let (tiny, ct) = run(4, Some(1));
        assert_eq!(one, four);
        assert_eq!(one, tiny);
        assert_eq!((c1, c4, ct), (2, 2, 2 * 40), "40 tiles, two passes");
    }

    /// Rows cross [`BATCH_SIZE`] and chunk boundaries (the carry), with
    /// points in the first batches and lines in the last, and a property
    /// that unifies Int + Float + Str: the file is the same however the
    /// tiles are chunked and threaded, and its footer reports every
    /// geometry type and a bbox spanning every tile.
    #[test]
    fn batches_cut_across_chunks_match_any_chunking() {
        use crate::pmtiles_writer::StreamingPmtilesWriter;
        use crate::vector_tile::tile::{Feature, Layer, Value};
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.pmtiles");
        let mut writer = StreamingPmtilesWriter::new(crate::Compression::Gzip).unwrap();
        writer.set_layer_name("t");
        for x in 0..5u32 {
            let features = (0..4000u32)
                .map(|i| Feature {
                    id: Some(u64::from(x * 4000 + i)),
                    tags: vec![0, i % 3],
                    r#type: Some(if x < 3 {
                        GeomType::Point as i32
                    } else {
                        GeomType::Linestring as i32
                    }),
                    geometry: if x < 3 {
                        vec![9, zigzag_encode((i % 4000) as i32), 2]
                    } else {
                        vec![9, 2, 2, 10, zigzag_encode((i % 100) as i32 + 1), 2]
                    },
                })
                .collect();
            let tile = Tile {
                layers: vec![Layer {
                    version: 2,
                    name: "t".into(),
                    features,
                    keys: vec!["k".into()],
                    values: vec![
                        Value {
                            int_value: Some(1),
                            ..Default::default()
                        },
                        Value {
                            double_value: Some(0.5),
                            ..Default::default()
                        },
                        Value {
                            string_value: Some("s".into()),
                            ..Default::default()
                        },
                    ],
                    extent: Some(4096),
                }],
            };
            writer.add_tile(6, x, 7, &tile.encode_to_vec()).unwrap();
        }
        writer.finalize(&input).unwrap();

        let run = |threads: usize, cap: Option<usize>| {
            let out = dir.path().join(format!("out{threads}-{cap:?}.parquet"));
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .unwrap();
            let report = pool.install(|| {
                CHUNK_BYTES_OVERRIDE.with(|c| c.set(cap));
                decode_pmtiles(&input, &out, &DecodeOptions::default()).unwrap()
            });
            assert_eq!(report.features_written, 20_000);
            std::fs::read(&out).unwrap()
        };
        let one = run(1, None);
        assert_eq!(one, run(4, Some(1)));
        assert_eq!(one, run(3, Some(5000)));

        let reader = tylertoo_io::open_parquet(
            std::fs::File::open(dir.path().join("out1-None.parquet")).unwrap(),
        )
        .unwrap();
        let meta = GeoParquetMetadata::from_parquet_meta(reader.metadata().file_metadata())
            .unwrap()
            .unwrap();
        let col = &meta.columns[crate::resolved_schema::DEFAULT_GEOMETRY_COLUMN];
        let mut types: Vec<String> = col.geometry_types.iter().map(|t| t.to_string()).collect();
        types.sort();
        assert_eq!(types, ["LineString", "Point"]);
        let bbox = col.bbox.as_ref().unwrap();
        let west = tile_local_to_lonlat(6, 0, 7, 4096, 0, 1).0;
        let east = tile_local_to_lonlat(6, 4, 7, 4096, 101, 1).0;
        assert!(
            (bbox[0] - west).abs() < 1e-9 && (bbox[2] - east).abs() < 1e-9,
            "{bbox:?}"
        );
    }
}
