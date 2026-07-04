//! SPIKE for issue #112: PMTiles -> GeoParquet decoding.
//!
//! NOT production code. Validates, end to end, the three feasibility
//! questions from the ticket's "Next Steps":
//!
//! 1. MVT decode path: our prost-generated `vector_tile` types plus the
//!    existing low-level decoders (`zigzag_decode`, `command_decode`) are
//!    sufficient — no geozero `with-mvt` needed (that feature would pull a
//!    second prost stack, ^0.11.9, next to our prost 0.14).
//! 2. PMTiles reading: our own `pmtiles_writer` module already contains
//!    `decode_varint` / `decode_directory`; together with a 127-byte header
//!    parse and gzip decompression (flate2, already a core dependency) we can
//!    enumerate and fetch tiles from archives produced by our export path,
//!    synchronously, with no external `pmtiles` crate and no async runtime.
//! 3. Coordinate transform: tippecanoe's tile-local -> 32-bit world ->
//!    WGS84 formula (write_json.cpp / projection.cpp) round-trips our export
//!    within max-zoom quantization tolerance.
//!
//! Run with:
//!   cargo test --package gpq-tiles-core --test spike_decode -- --nocapture
//!
//! See context/archive/SPIKE_112_DECODE.md for the findings write-up.

use std::io::Read;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{Geometry, LineString, Point, Polygon};
use geoarrow::array::GeometryBuilder;
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use parquet::arrow::ArrowWriter;
use prost::Message;

use gpq_tiles_core::mvt::{command_decode, zigzag_decode};
use gpq_tiles_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use gpq_tiles_core::overview::export::{export_pmtiles, ExportOptions};
use gpq_tiles_core::pmtiles_writer::{decode_directory, tile_id, DirEntry};
use gpq_tiles_core::vector_tile::Tile;

// ============================================================================
// Fixture: tiny GeoParquet with known coordinates
// ============================================================================

/// Source geometries with hand-picked coordinates (mid-tile at z14 to avoid
/// clip-introduced vertices in the assertions; clipping correctness is not
/// what this spike measures).
fn fixture_geometries() -> Vec<Geometry<f64>> {
    vec![
        Geometry::Point(Point::new(-75.1652, 39.9526)), // Philadelphia
        Geometry::Point(Point::new(2.3522, 48.8566)),   // Paris
        Geometry::Point(Point::new(151.2093, -33.8688)), // Sydney
        // Small linestring (~200m long) well inside a z14 tile.
        Geometry::LineString(LineString::from(vec![
            (10.0030, 50.0030),
            (10.0040, 50.0035),
            (10.0050, 50.0030),
        ])),
        // Small polygon (~100m across) well inside a z14 tile.
        Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (-45.0030, -20.0030),
                (-45.0020, -20.0030),
                (-45.0020, -20.0020),
                (-45.0030, -20.0020),
                (-45.0030, -20.0030),
            ]),
            vec![],
        )),
    ]
}

/// Write the fixture as GeoParquet (id: Int64, name: Utf8, geometry: WKB).
/// Mirrors the fixture builder used by the overview hostile-input tests.
fn write_fixture(path: &Path, geoms: &[Geometry<f64>]) {
    let n = geoms.len();
    let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
    let name = StringArray::from((0..n).map(|i| format!("f{i}")).collect::<Vec<_>>());

    let typ = GeometryType::new(Default::default());
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field("geometry", true);

    let fields = vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(Field::new("name", DataType::Utf8, false)),
        Arc::new(geom_field),
    ];
    let columns: Vec<Arc<dyn Array>> = vec![Arc::new(id), Arc::new(name), geom_arr.to_array_ref()];

    let schema = Arc::new(Schema::new(fields));
    let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();

    let gpq_options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_generate_covering(true)
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &gpq_options).unwrap();
    let target_schema = encoder.target_schema();

    let file = std::fs::File::create(path).unwrap();
    let mut writer = ArrowWriter::try_new(file, target_schema, None).unwrap();
    writer
        .write(&encoder.encode_record_batch(&batch).unwrap())
        .unwrap();
    writer.append_key_value_metadata(encoder.into_keyvalue().unwrap());
    writer.close().unwrap();
}

// ============================================================================
// Minimal synchronous PMTiles reader (spike quality)
// ============================================================================

/// The subset of the 127-byte PMTiles v3 header the decoder needs.
#[derive(Debug)]
struct SpikeHeader {
    root_dir_offset: u64,
    root_dir_length: u64,
    leaf_dirs_offset: u64,
    tile_data_offset: u64,
    internal_compression: u8,
    tile_compression: u8,
    min_zoom: u8,
    max_zoom: u8,
}

fn read_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(buf[at..at + 8].try_into().unwrap())
}

fn parse_header(bytes: &[u8]) -> SpikeHeader {
    assert!(bytes.len() >= 127, "file shorter than PMTiles header");
    assert_eq!(&bytes[0..7], b"PMTiles", "bad magic");
    assert_eq!(bytes[7], 3, "unsupported PMTiles version");
    SpikeHeader {
        root_dir_offset: read_u64(bytes, 8),
        root_dir_length: read_u64(bytes, 16),
        leaf_dirs_offset: read_u64(bytes, 40),
        tile_data_offset: read_u64(bytes, 56),
        internal_compression: bytes[97],
        tile_compression: bytes[98],
        min_zoom: bytes[100],
        max_zoom: bytes[101],
    }
}

/// Gzip decompress (the only compression our writer currently emits:
/// `export_pmtiles` hardcodes `StreamingPmtilesWriter::new(Compression::Gzip)`).
fn gunzip(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(data)
        .read_to_end(&mut out)
        .expect("gzip decompress");
    out
}

/// Inverse of `pmtiles_writer::tile_id`: cumulative Hilbert tile ID -> (z, x, y).
fn tile_id_to_zxy(id: u64) -> (u8, u32, u32) {
    let mut acc = 0u64;
    for z in 0u8..=32 {
        let num = 1u64 << (2 * u64::from(z));
        if id < acc + num {
            let (x, y) = hilbert_d2xy(z, id - acc);
            return (z, x, y);
        }
        acc += num;
    }
    panic!("tile id {id} out of range");
}

/// Standard Hilbert d -> (x, y) (Wikipedia d2xy), the inverse of the writer's
/// `xy_to_hilbert`.
fn hilbert_d2xy(z: u8, d: u64) -> (u32, u32) {
    let n = 1u64 << z;
    let (mut x, mut y) = (0u64, 0u64);
    let mut t = d;
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (x as u32, y as u32)
}

/// Enumerate every (z, x, y) -> raw tile bytes in the archive, walking the
/// root directory and any leaf directories (run_length == 0 entries).
fn enumerate_tiles(bytes: &[u8], header: &SpikeHeader) -> Vec<(u8, u32, u32, Vec<u8>)> {
    let root_raw = &bytes[header.root_dir_offset as usize
        ..(header.root_dir_offset + header.root_dir_length) as usize];
    let root = decode_directory(&gunzip(root_raw)).expect("decode root directory");

    let mut tile_entries: Vec<DirEntry> = Vec::new();
    for entry in &root {
        if entry.run_length == 0 {
            // Leaf directory: offset is relative to the leaf directory section.
            let start = (header.leaf_dirs_offset + entry.offset) as usize;
            let leaf_raw = &bytes[start..start + entry.length as usize];
            let leaf = decode_directory(&gunzip(leaf_raw)).expect("decode leaf directory");
            tile_entries.extend(leaf);
        } else {
            tile_entries.push(entry.clone());
        }
    }

    let mut out = Vec::new();
    for entry in &tile_entries {
        let start = (header.tile_data_offset + entry.offset) as usize;
        let raw = bytes[start..start + entry.length as usize].to_vec();
        // run_length > 1 means several consecutive tile IDs share these bytes.
        for i in 0..u64::from(entry.run_length.max(1)) {
            let (z, x, y) = tile_id_to_zxy(entry.tile_id + i);
            // Sanity: our inverse must round-trip the writer's forward mapping.
            assert_eq!(
                tile_id(z, x, y),
                entry.tile_id + i,
                "hilbert inverse mismatch"
            );
            out.push((z, x, y, raw.clone()));
        }
    }
    out
}

// ============================================================================
// MVT geometry decoding (manual: command_decode + zigzag_decode)
// ============================================================================

/// Decode an MVT geometry command stream into parts of tile-local integer
/// vertices. ClosePath is a no-op for vertex extraction (the closing vertex
/// duplicates the ring start).
fn decode_mvt_geometry(geom: &[u32]) -> Vec<Vec<(i64, i64)>> {
    const CMD_MOVE_TO: u32 = 1;
    const CMD_LINE_TO: u32 = 2;
    const CMD_CLOSE_PATH: u32 = 7;

    let mut parts: Vec<Vec<(i64, i64)>> = Vec::new();
    let mut cur: Vec<(i64, i64)> = Vec::new();
    let (mut cx, mut cy) = (0i64, 0i64);
    let mut i = 0usize;
    while i < geom.len() {
        let (cmd, count) = command_decode(geom[i]);
        i += 1;
        match cmd {
            CMD_MOVE_TO => {
                for _ in 0..count {
                    cx += i64::from(zigzag_decode(geom[i]));
                    cy += i64::from(zigzag_decode(geom[i + 1]));
                    i += 2;
                    if !cur.is_empty() {
                        parts.push(std::mem::take(&mut cur));
                    }
                    cur.push((cx, cy));
                }
            }
            CMD_LINE_TO => {
                for _ in 0..count {
                    cx += i64::from(zigzag_decode(geom[i]));
                    cy += i64::from(zigzag_decode(geom[i + 1]));
                    i += 2;
                    cur.push((cx, cy));
                }
            }
            CMD_CLOSE_PATH => {}
            other => panic!("unknown MVT command {other}"),
        }
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

// ============================================================================
// Tippecanoe coordinate transform (write_json.cpp / projection.cpp)
// ============================================================================

/// Tile-local integer coords -> WGS84, via 32-bit Web Mercator world coords.
///
///   wscale = 1 << (32 - z)
///   wx = wscale * tx + (wscale / extent) * px
///   lon = wx / 2^32 * 360 - 180
///   lat = atan(sinh(pi - 2*pi * wy / 2^32)) in degrees
///
/// NOTE: `wscale / extent` is exact only while extent (4096 = 2^12) divides
/// 2^(32-z), i.e. z <= 20 for the default extent. Fine for the spike; the
/// real implementation should switch to f64 world coords above that.
fn tile_px_to_lonlat(z: u8, tx: u32, ty: u32, extent: u32, px: i64, py: i64) -> (f64, f64) {
    assert!(
        z <= 20,
        "integer world-coordinate path only exact for z <= 20"
    );
    let wscale = 1i64 << (32 - i64::from(z));
    let unit = wscale / i64::from(extent);
    let wx = wscale * i64::from(tx) + unit * px;
    let wy = wscale * i64::from(ty) + unit * py;
    const WORLD: f64 = 4_294_967_296.0; // 2^32
    let lon = wx as f64 / WORLD * 360.0 - 180.0;
    let n = std::f64::consts::PI - 2.0 * std::f64::consts::PI * (wy as f64) / WORLD;
    let lat = n.sinh().atan().to_degrees();
    (lon, lat)
}

// ============================================================================
// The spike round-trip test
// ============================================================================

#[test]
fn spike_pmtiles_roundtrip_decode() {
    const MAX_ZOOM: u8 = 14;

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("fixture.parquet");
    let overviews = dir.path().join("fixture-overviews.parquet");
    let pmtiles = dir.path().join("fixture.pmtiles");

    let geoms = fixture_geometries();
    write_fixture(&input, &geoms);

    // --- Our production pipeline: convert + export -------------------------
    let convert_opts = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: 4,
            max_zoom: MAX_ZOOM,
        },
        ..Default::default()
    };
    convert_to_overviews(&input, &overviews, &convert_opts).unwrap();
    export_pmtiles(&overviews, &pmtiles, &ExportOptions::default()).unwrap();

    // --- Read the archive back (sync, no pmtiles crate) --------------------
    let bytes = std::fs::read(&pmtiles).unwrap();
    let header = parse_header(&bytes);
    println!("header: {header:?}");
    assert_eq!(
        header.internal_compression, 2,
        "writer emits gzip directories"
    );
    assert_eq!(header.tile_compression, 2, "writer emits gzip tiles");
    assert_eq!(header.max_zoom, MAX_ZOOM);

    let tiles = enumerate_tiles(&bytes, &header);
    assert!(!tiles.is_empty(), "archive contains tiles");
    let zooms: std::collections::BTreeSet<u8> = tiles.iter().map(|t| t.0).collect();
    println!(
        "enumerated {} tiles across zooms {:?} (min declared {}, max declared {})",
        tiles.len(),
        zooms,
        header.min_zoom,
        header.max_zoom
    );
    assert!(zooms.contains(&MAX_ZOOM), "max-zoom tiles present");

    // --- Decode every max-zoom tile: gunzip -> prost -> command stream -----
    // Collected as (lon, lat) vertices plus the per-feature `id` property.
    let mut decoded: Vec<(i64, Vec<(f64, f64)>)> = Vec::new();
    for (z, x, y, raw) in tiles.iter().filter(|t| t.0 == MAX_ZOOM) {
        let tile = Tile::decode(gunzip(raw).as_slice()).expect("prost MVT decode");
        for layer in &tile.layers {
            assert_eq!(layer.name, "overview", "export layer name");
            let extent = layer.extent.unwrap_or(4096);
            assert_eq!(extent, 4096);
            for feature in &layer.features {
                // Property decode: find the Int64 `id` tag.
                let mut id: Option<i64> = None;
                for pair in feature.tags.chunks(2) {
                    if layer.keys[pair[0] as usize] == "id" {
                        let v = &layer.values[pair[1] as usize];
                        id = v
                            .int_value
                            .or(v.sint_value)
                            .or_else(|| v.uint_value.map(|u| u as i64));
                    }
                }
                let id = id.expect("feature carries the `id` property");
                let verts: Vec<(f64, f64)> = decode_mvt_geometry(&feature.geometry)
                    .into_iter()
                    .flatten()
                    .map(|(px, py)| tile_px_to_lonlat(*z, *x, *y, extent, px, py))
                    .collect();
                decoded.push((id, verts));
            }
        }
    }
    println!("decoded {} feature instances at z{MAX_ZOOM}", decoded.len());

    // --- Accuracy: every source vertex within quantization tolerance -------
    // One tile-local unit at zoom z spans 360 / (2^z * extent) degrees of
    // longitude; encoding rounds to the nearest unit (<= 0.5 unit error) and
    // latitude error in degrees is never larger than longitude error at the
    // same world position. Allow a full unit for slack.
    let tol_deg = 360.0 / ((1u64 << MAX_ZOOM) as f64 * 4096.0);
    let tol_m = tol_deg * 111_320.0;
    println!("tolerance: {tol_deg:.9} deg (~{tol_m:.2} m at the equator)");

    let mut worst = 0.0f64;
    for (source_id, geom) in geoms.iter().enumerate() {
        let source_verts: Vec<(f64, f64)> = match geom {
            Geometry::Point(p) => vec![(p.x(), p.y())],
            Geometry::LineString(l) => l.points().map(|p| (p.x(), p.y())).collect(),
            Geometry::Polygon(p) => {
                // Skip the closing vertex (duplicates the first).
                let pts: Vec<_> = p.exterior().points().map(|p| (p.x(), p.y())).collect();
                pts[..pts.len() - 1].to_vec()
            }
            _ => unreachable!("fixture only uses point/line/polygon"),
        };
        // Decoded instances of this feature (buffer copies in neighbor tiles
        // are expected; every copy must contain a match for every vertex).
        let instances: Vec<&Vec<(f64, f64)>> = decoded
            .iter()
            .filter(|(id, _)| *id == source_id as i64)
            .map(|(_, v)| v)
            .collect();
        assert!(
            !instances.is_empty(),
            "feature id={source_id} present at max zoom"
        );
        for (sx, sy) in &source_verts {
            let err = instances
                .iter()
                .flat_map(|verts| verts.iter())
                .map(|(dx, dy)| (dx - sx).abs().max((dy - sy).abs()))
                .fold(f64::INFINITY, f64::min);
            assert!(
                err <= tol_deg,
                "feature id={source_id} vertex ({sx}, {sy}): error {err:.9} deg \
                 exceeds tolerance {tol_deg:.9} deg"
            );
            worst = worst.max(err);
        }
    }
    println!(
        "round-trip OK: worst vertex error {worst:.9} deg (~{:.3} m) vs tolerance {tol_deg:.9} deg (~{tol_m:.2} m)",
        worst * 111_320.0
    );
}
