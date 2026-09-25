//! Integration tests for PMTiles → GeoParquet decoding (issue #112).
//!
//! Round-trips through the real production pipeline: fixture GeoParquet →
//! `convert_to_overviews` → `export_pmtiles` → `decode_pmtiles` → read the
//! output GeoParquet and verify coordinates, provenance columns, properties
//! and filters. Plus a golden comparison against `tippecanoe-decode` when
//! that binary is available (skipped gracefully otherwise).
//!
//! Run with:
//!   cargo test --package tylertoo-core --test decode_roundtrip -- --nocapture

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use arrow_array::cast::AsArray;
use arrow_array::types::{Int64Type, UInt64Type, UInt8Type};
use arrow_array::{Array, Int64Array, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use geo::{Geometry, LineString, Point, Polygon};
use geoarrow::array::{from_arrow_array, GeometryBuilder};
use geoarrow::datatypes::GeometryType;
use geoarrow_array::GeoArrowArray;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;

use tylertoo_core::batch_processor::extract_geometries_from_array;
use tylertoo_core::decode::{decode_pmtiles, DecodeError, DecodeOptions};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

const MIN_ZOOM: u8 = 4;
const MAX_ZOOM: u8 = 14;

/// Quantization tolerance in degrees at (zoom, extent 4096): one tile-local
/// unit of longitude. Encoding rounds to the nearest unit (<= 0.5 units);
/// latitude error in degrees never exceeds longitude error at the same
/// world position.
fn tol_deg(zoom: u8) -> f64 {
    360.0 / ((1u64 << zoom) as f64 * 4096.0)
}

/// Source geometries with hand-picked coordinates (mid-tile at z14, so
/// clipping introduces no extra vertices in what we assert on).
fn fixture_geometries() -> Vec<Geometry<f64>> {
    vec![
        Geometry::Point(Point::new(-75.1652, 39.9526)), // Philadelphia
        Geometry::Point(Point::new(2.3522, 48.8566)),   // Paris
        Geometry::Point(Point::new(151.2093, -33.8688)), // Sydney
        Geometry::LineString(LineString::from(vec![
            (10.0030, 50.0030),
            (10.0040, 50.0035),
            (10.0050, 50.0030),
        ])),
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

/// Write a GeoParquet fixture with `id` (Int64) and `name` (Utf8) properties,
/// plus an optional extra Int64 column (reserved-name collision tests).
fn write_fixture(path: &Path, geoms: &[Geometry<f64>], extra_col: Option<&str>) {
    let n = geoms.len();
    let id = Int64Array::from((0..n as i64).collect::<Vec<_>>());
    let name = StringArray::from((0..n).map(|i| format!("f{i}")).collect::<Vec<_>>());

    let typ = GeometryType::new(Default::default());
    let mut b = GeometryBuilder::new(typ).with_prefer_multi(false);
    b.extend_from_iter(geoms.iter().map(Some));
    let geom_arr = b.finish();
    let geom_field = geom_arr.data_type().to_field("geometry", true);

    let mut fields = vec![
        Arc::new(Field::new("id", DataType::Int64, false)),
        Arc::new(Field::new("name", DataType::Utf8, false)),
    ];
    let mut columns: Vec<Arc<dyn Array>> = vec![Arc::new(id), Arc::new(name)];
    if let Some(col) = extra_col {
        fields.push(Arc::new(Field::new(col, DataType::Int64, false)));
        columns.push(Arc::new(Int64Array::from(vec![7i64; n])));
    }
    fields.push(Arc::new(geom_field));
    columns.push(geom_arr.to_array_ref());

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

/// Run the production pipeline (convert + export) over the fixture and
/// return the PMTiles path (tempdir keeps everything alive).
fn build_archive(dir: &tempfile::TempDir, extra_col: Option<&str>) -> PathBuf {
    let input = dir.path().join("fixture.parquet");
    let overviews = dir.path().join("fixture-overviews.parquet");
    let pmtiles = dir.path().join("fixture.pmtiles");

    write_fixture(&input, &fixture_geometries(), extra_col);
    let convert_opts = ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: MIN_ZOOM,
            max_zoom: MAX_ZOOM,
        },
        ..Default::default()
    };
    convert_to_overviews(&input, &overviews, &convert_opts).unwrap();
    export_pmtiles(&overviews, &pmtiles, &ExportOptions::default()).unwrap();
    pmtiles
}

/// A decoded output row: provenance + properties + geometry.
struct Row {
    zoom: u8,
    layer: String,
    mvt_id: Option<u64>,
    id: Option<i64>,
    name: Option<String>,
    geometry: Geometry<f64>,
}

/// Read every row of a decoded GeoParquet file.
fn read_rows(path: &Path) -> Vec<Row> {
    let file = std::fs::File::open(path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let mut rows = Vec::new();
    for batch in builder.build().unwrap() {
        let batch = batch.unwrap();
        let schema = batch.schema();
        let zoom = batch
            .column(schema.index_of("zoom").unwrap())
            .as_primitive::<UInt8Type>()
            .clone();
        let layer = batch
            .column(schema.index_of("layer").unwrap())
            .as_string::<i32>()
            .clone();
        let mvt_id = batch
            .column(schema.index_of("mvt_id").unwrap())
            .as_primitive::<UInt64Type>()
            .clone();
        let id = batch
            .column(schema.index_of("id").unwrap())
            .as_primitive::<Int64Type>()
            .clone();
        let name = batch
            .column(schema.index_of("name").unwrap())
            .as_string::<i32>()
            .clone();
        let gidx = schema.index_of("geometry").unwrap();
        let garr = from_arrow_array(batch.column(gidx).as_ref(), schema.field(gidx)).unwrap();
        let mut geoms = Vec::new();
        extract_geometries_from_array(garr.as_ref(), &mut geoms).unwrap();
        assert_eq!(geoms.len(), batch.num_rows(), "no null geometry rows");
        for (i, geometry) in geoms.into_iter().enumerate() {
            rows.push(Row {
                zoom: zoom.value(i),
                layer: layer.value(i).to_string(),
                mvt_id: (!mvt_id.is_null(i)).then(|| mvt_id.value(i)),
                id: (!id.is_null(i)).then(|| id.value(i)),
                name: (!name.is_null(i)).then(|| name.value(i).to_string()),
                geometry,
            });
        }
    }
    rows
}

/// All vertices of a geometry as (lon, lat) pairs.
fn vertices(geom: &Geometry<f64>) -> Vec<(f64, f64)> {
    use geo::CoordsIter;
    geom.coords_iter().map(|c| (c.x, c.y)).collect()
}

// ============================================================================
// Round-trip
// ============================================================================

#[test]
fn decode_roundtrip_full_archive() {
    let dir = tempfile::tempdir().unwrap();
    let pmtiles = build_archive(&dir, None);
    let output = dir.path().join("decoded.parquet");

    let report = decode_pmtiles(&pmtiles, &output, &DecodeOptions::default()).unwrap();
    println!("report: {report:?}");

    let rows = read_rows(&output);
    assert_eq!(rows.len() as u64, report.features_written);
    assert!(report.features_written > 0);
    assert_eq!(report.layers, vec!["overview".to_string()]);
    assert_eq!(report.zoom_range, Some((MIN_ZOOM, MAX_ZOOM)));

    // Provenance columns.
    let zooms: BTreeSet<u8> = rows.iter().map(|r| r.zoom).collect();
    assert!(zooms.contains(&MAX_ZOOM), "max-zoom rows present");
    assert!(*zooms.iter().min().unwrap() >= MIN_ZOOM);
    assert!(rows.iter().all(|r| r.layer == "overview"));
    // Our export writes per-tile sequential MVT feature ids.
    assert!(rows.iter().all(|r| r.mvt_id.is_some()));

    // Properties survive: id and name paired as in the source.
    for row in &rows {
        let id = row.id.expect("id property present");
        assert_eq!(row.name.as_deref(), Some(format!("f{id}").as_str()));
    }

    // Coordinate accuracy at max zoom: every source vertex must have a
    // decoded counterpart within quantization tolerance.
    let tol = tol_deg(MAX_ZOOM);
    let mut worst = 0.0f64;
    for (source_id, geom) in fixture_geometries().iter().enumerate() {
        let instances: Vec<&Row> = rows
            .iter()
            .filter(|r| r.zoom == MAX_ZOOM && r.id == Some(source_id as i64))
            .collect();
        assert!(!instances.is_empty(), "feature {source_id} at max zoom");
        for (sx, sy) in vertices(geom) {
            let err = instances
                .iter()
                .flat_map(|r| vertices(&r.geometry))
                .map(|(dx, dy)| (dx - sx).abs().max((dy - sy).abs()))
                .fold(f64::INFINITY, f64::min);
            assert!(
                err <= tol,
                "feature {source_id} vertex ({sx}, {sy}): error {err} > tol {tol}"
            );
            worst = worst.max(err);
        }
    }
    println!(
        "round-trip OK: worst vertex error {worst:.9} deg (~{:.3} m) vs tol {tol:.9} deg",
        worst * 111_320.0
    );

    // Geometry types survive at max zoom (no simplification at the finest
    // level; polygons stay polygons, lines stay lines).
    for row in rows.iter().filter(|r| r.zoom == MAX_ZOOM) {
        match row.id.unwrap() {
            0..=2 => assert!(matches!(row.geometry, Geometry::Point(_))),
            3 => assert!(matches!(
                row.geometry,
                Geometry::LineString(_) | Geometry::MultiLineString(_)
            )),
            4 => assert!(matches!(
                row.geometry,
                Geometry::Polygon(_) | Geometry::MultiPolygon(_)
            )),
            other => panic!("unexpected id {other}"),
        }
    }
}

// ============================================================================
// Filters
// ============================================================================

#[test]
fn decode_single_zoom_filter() {
    let dir = tempfile::tempdir().unwrap();
    let pmtiles = build_archive(&dir, None);
    let output = dir.path().join("decoded-z8.parquet");

    let options = DecodeOptions {
        min_zoom: Some(8),
        max_zoom: Some(8),
        layer: None,
    };
    let report = decode_pmtiles(&pmtiles, &output, &options).unwrap();
    let rows = read_rows(&output);

    assert_eq!(rows.len() as u64, report.features_written);
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r.zoom == 8), "only z8 rows");
    assert_eq!(report.zoom_range, Some((8, 8)));
}

#[test]
fn decode_zoom_range_filter() {
    let dir = tempfile::tempdir().unwrap();
    let pmtiles = build_archive(&dir, None);
    let output = dir.path().join("decoded-z10-12.parquet");

    let options = DecodeOptions {
        min_zoom: Some(10),
        max_zoom: Some(12),
        layer: None,
    };
    decode_pmtiles(&pmtiles, &output, &options).unwrap();
    let rows = read_rows(&output);
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| (10..=12).contains(&r.zoom)));
}

#[test]
fn decode_layer_filter_matching_and_not() {
    let dir = tempfile::tempdir().unwrap();
    let pmtiles = build_archive(&dir, None);

    // Matching layer: same rows as unfiltered.
    let out_match = dir.path().join("decoded-layer.parquet");
    let report = decode_pmtiles(
        &pmtiles,
        &out_match,
        &DecodeOptions {
            layer: Some("overview".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(report.features_written > 0);

    // Non-matching layer: empty (but valid) output.
    let out_none = dir.path().join("decoded-nolayer.parquet");
    let report = decode_pmtiles(
        &pmtiles,
        &out_none,
        &DecodeOptions {
            layer: Some("does-not-exist".to_string()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(report.features_written, 0);
    assert!(report.layers.is_empty());
    assert_eq!(report.zoom_range, None);

    // The empty file must still be readable parquet with the provenance
    // schema (zero rows).
    let file = std::fs::File::open(&out_none).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    let names: Vec<String> = builder
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect();
    assert!(names.contains(&"zoom".to_string()));
    assert!(names.contains(&"layer".to_string()));
    assert!(names.contains(&"mvt_id".to_string()));
    assert!(names.contains(&"geometry".to_string()));
    assert_eq!(builder.build().unwrap().count(), 0);
}

// ============================================================================
// Errors
// ============================================================================

#[test]
fn decode_rejects_reserved_property_name() {
    // A source property named `mvt_id` collides with the decoder's
    // provenance column and must be rejected, not silently clobbered.
    let dir = tempfile::tempdir().unwrap();
    let pmtiles = build_archive(&dir, Some("mvt_id"));
    let output = dir.path().join("decoded.parquet");

    let err = decode_pmtiles(&pmtiles, &output, &DecodeOptions::default()).unwrap_err();
    assert!(
        matches!(err, DecodeError::ReservedColumn(ref c) if c == "mvt_id"),
        "expected ReservedColumn(mvt_id), got {err:?}"
    );
}

#[test]
fn decode_rejects_non_pmtiles_input() {
    let dir = tempfile::tempdir().unwrap();
    let garbage = dir.path().join("garbage.pmtiles");
    std::fs::write(
        &garbage,
        b"definitely not a pmtiles archive, but long enough to parse",
    )
    .unwrap();
    let output = dir.path().join("decoded.parquet");
    assert!(decode_pmtiles(&garbage, &output, &DecodeOptions::default()).is_err());
}

#[test]
fn decode_rejects_missing_input() {
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("decoded.parquet");
    assert!(decode_pmtiles(
        dir.path().join("nope.pmtiles"),
        &output,
        &DecodeOptions::default()
    )
    .is_err());
}

// ============================================================================
// Golden comparison against tippecanoe-decode
// ============================================================================

/// Recursively collect `(geometry-type, [vertices])` for every Feature in a
/// tippecanoe-decode GeoJSON document.
fn collect_tippecanoe_features(
    value: &serde_json::Value,
    out: &mut Vec<(String, Vec<(f64, f64)>)>,
) {
    match value {
        serde_json::Value::Object(obj) => {
            if obj.get("type").and_then(|t| t.as_str()) == Some("Feature") {
                if let Some(geom) = obj.get("geometry") {
                    let gtype = geom
                        .get("type")
                        .and_then(|t| t.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let mut verts = Vec::new();
                    collect_positions(
                        geom.get("coordinates").unwrap_or(&serde_json::Value::Null),
                        &mut verts,
                    );
                    out.push((gtype, verts));
                }
            }
            for v in obj.values() {
                collect_tippecanoe_features(v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            for v in arr {
                collect_tippecanoe_features(v, out);
            }
        }
        _ => {}
    }
}

/// Flatten arbitrarily nested GeoJSON coordinates into (lon, lat) pairs.
fn collect_positions(value: &serde_json::Value, out: &mut Vec<(f64, f64)>) {
    if let serde_json::Value::Array(arr) = value {
        if arr.len() >= 2 && arr[0].is_number() && arr[1].is_number() {
            out.push((arr[0].as_f64().expect("lon"), arr[1].as_f64().expect("lat")));
        } else {
            for v in arr {
                collect_positions(v, out);
            }
        }
    }
}

#[test]
fn decode_golden_against_tippecanoe_decode() {
    // Skip gracefully when the reference binary is unavailable.
    if Command::new("tippecanoe-decode")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("tippecanoe-decode not available, skipping golden comparison");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let pmtiles = build_archive(&dir, None);
    let output = dir.path().join("decoded.parquet");

    // Ours, max zoom only.
    let options = DecodeOptions {
        min_zoom: Some(MAX_ZOOM),
        max_zoom: Some(MAX_ZOOM),
        layer: None,
    };
    decode_pmtiles(&pmtiles, &output, &options).unwrap();
    let ours = read_rows(&output);

    // Reference, max zoom only.
    let ref_out = Command::new("tippecanoe-decode")
        .arg("-Z")
        .arg(MAX_ZOOM.to_string())
        .arg("-z")
        .arg(MAX_ZOOM.to_string())
        .arg(&pmtiles)
        .output()
        .expect("run tippecanoe-decode");
    assert!(
        ref_out.status.success(),
        "tippecanoe-decode failed: {}",
        String::from_utf8_lossy(&ref_out.stderr)
    );
    let doc: serde_json::Value =
        serde_json::from_slice(&ref_out.stdout).expect("tippecanoe-decode emits JSON");
    let mut reference = Vec::new();
    collect_tippecanoe_features(&doc, &mut reference);

    // Same number of feature instances at max zoom.
    assert_eq!(
        ours.len(),
        reference.len(),
        "feature count at z{MAX_ZOOM}: ours {} vs tippecanoe-decode {}",
        ours.len(),
        reference.len()
    );

    // Every vertex we decode must appear in tippecanoe-decode's output for
    // the same zoom, within a hair of float printing error (both sides
    // implement the same integer world-coordinate transform).
    let tol = 1e-6;
    for row in &ours {
        for (x, y) in vertices(&row.geometry) {
            let matched = reference
                .iter()
                .flat_map(|(_, verts)| verts.iter())
                .any(|&(rx, ry)| (rx - x).abs() <= tol && (ry - y).abs() <= tol);
            assert!(
                matched,
                "vertex ({x}, {y}) from row id {:?} not found in tippecanoe-decode output",
                row.id
            );
        }
    }
    println!(
        "golden OK: {} features at z{MAX_ZOOM} match tippecanoe-decode",
        ours.len()
    );
}

// ---------------------------------------------------------------------------
// Foreign root → leaf layout (#377 / #382)
// ---------------------------------------------------------------------------

/// Protobuf varint.
fn varint(mut v: u64, out: &mut Vec<u8>) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// A minimal MVT tile: one layer `t` (extent 4096, version 2) holding one
/// point feature at tile-local `(x, y)`. Distinct coordinates give distinct
/// tile bytes, so the writer cannot collapse the tiles into runs.
fn mvt_point_tile(x: u32, y: u32) -> Vec<u8> {
    let zigzag = |v: u32| u64::from(v) << 1; // non-negative → zigzag is v*2
    let mut geom = Vec::new();
    varint(9, &mut geom); // MoveTo, count 1
    varint(zigzag(x), &mut geom);
    varint(zigzag(y), &mut geom);

    let mut feature = vec![0x18, 0x01]; // type = POINT
    feature.push(0x22); // geometry (packed)
    varint(geom.len() as u64, &mut feature);
    feature.extend_from_slice(&geom);

    let mut layer = vec![0x0A, 0x01, b't']; // name = "t"
    layer.push(0x12); // features
    varint(feature.len() as u64, &mut layer);
    layer.extend_from_slice(&feature);
    layer.extend_from_slice(&[0x28, 0x80, 0x20]); // extent = 4096
    layer.extend_from_slice(&[0x78, 0x02]); // version = 2

    let mut tile = vec![0x1A]; // layers
    varint(layer.len() as u64, &mut tile);
    tile.extend_from_slice(&layer);
    tile
}

/// Re-encode a directory of leaf pointers the way tippecanoe / go-pmtiles
/// write a root: every offset after the first is the contiguous marker 0.
/// Independent of `encode_directory` on purpose — this pins the on-disk
/// shape that other writers produce, whatever our encoder does.
fn tippecanoe_style_root(entries: &[tylertoo_core::pmtiles_writer::DirEntry]) -> Vec<u8> {
    let mut buf = Vec::new();
    varint(entries.len() as u64, &mut buf);
    let mut last_id = 0;
    for e in entries {
        varint(e.tile_id - last_id, &mut buf);
        last_id = e.tile_id;
    }
    for e in entries {
        varint(u64::from(e.run_length), &mut buf);
    }
    for e in entries {
        varint(u64::from(e.length), &mut buf);
    }
    for (i, e) in entries.iter().enumerate() {
        if i == 0 {
            varint(e.offset + 1, &mut buf);
        } else {
            let prev = &entries[i - 1];
            assert_eq!(
                e.offset,
                prev.offset + u64::from(prev.length),
                "leaf directories must be laid out back to back for this test"
            );
            varint(0, &mut buf);
        }
    }
    buf
}

/// Decode an archive whose root directory points at leaf directories using
/// the spec's contiguous-offset encoding — the layout tippecanoe and
/// go-pmtiles produce, and the one that failed with "incomplete deflate
/// stream" before #377. The archive is synthesized here: our writer supplies
/// the leaves and tile data, and the root is rewritten by hand.
#[test]
fn decode_reads_foreign_root_with_contiguous_leaf_offsets() {
    use tylertoo_core::compression::{compress, decompress_capped, MAX_INTERNAL_BYTES};
    use tylertoo_core::pmtiles_writer::{decode_directory, Header};
    use tylertoo_core::{Compression, StreamingPmtilesWriter};

    let dir = tempfile::tempdir().unwrap();
    let ours = dir.path().join("ours.pmtiles");

    // Enough distinct tiles, scattered widely enough, that the *compressed*
    // root exceeds the 16 KB budget and overflows into leaf directories. A
    // regular block of tiles has near-zero entropy in its id deltas and
    // gzips into the root no matter how many there are.
    const N: u32 = 12_000;
    const Z: u8 = 13;
    let mut writer = StreamingPmtilesWriter::new(Compression::Gzip).unwrap();
    writer.set_layer_name("t");
    let mut seen = std::collections::HashSet::new();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    while seen.len() < N as usize {
        // xorshift64*: deterministic, no dependency
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let r = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        let (x, y) = ((r & 0x1FFF) as u32, ((r >> 13) & 0x1FFF) as u32);
        if seen.insert((x, y)) {
            writer
                .add_tile(Z, x, y, &mvt_point_tile(x % 4096, y % 4096))
                .unwrap();
        }
    }
    writer.finalize(&ours).unwrap();

    let bytes = std::fs::read(&ours).unwrap();
    let header = Header::from_bytes(&bytes[..127]).unwrap();
    assert!(
        header.leaf_dirs_length > 0,
        "fixture must have leaf directories, got none for {N} tiles"
    );
    let root_start = header.root_dir_offset as usize;
    let root_end = root_start + header.root_dir_length as usize;
    let root = decode_directory(
        &decompress_capped(
            &bytes[root_start..root_end],
            header.internal_compression,
            MAX_INTERNAL_BYTES,
        )
        .unwrap(),
    )
    .unwrap();
    assert!(root.len() > 1, "root must hold several leaf pointers");
    assert!(root.iter().all(|e| e.run_length == 0));

    // Splice in the foreign-style root. Its length differs from ours, so
    // every later section shifts by `delta`.
    let plain_root = tippecanoe_style_root(&root);
    // Our encoder now writes exactly this shape; pin that parity explicitly,
    // since the splice below is otherwise a no-op against our own archive.
    assert_eq!(
        tylertoo_core::pmtiles_writer::encode_directory(&root),
        plain_root,
        "encode_directory must write leaf pointers with contiguous offsets"
    );
    let foreign_root = compress(&plain_root, header.internal_compression).unwrap();
    let delta = foreign_root.len() as i64 - header.root_dir_length as i64;
    let shift = |off: u64| (off as i64 + delta) as u64;
    let foreign_header = Header {
        root_dir_length: foreign_root.len() as u64,
        json_metadata_offset: shift(header.json_metadata_offset),
        leaf_dirs_offset: shift(header.leaf_dirs_offset),
        tile_data_offset: shift(header.tile_data_offset),
        ..header
    };
    let mut foreign = Vec::with_capacity(bytes.len());
    foreign.extend_from_slice(&foreign_header.to_bytes());
    foreign.extend_from_slice(&bytes[127..root_start]);
    foreign.extend_from_slice(&foreign_root);
    foreign.extend_from_slice(&bytes[root_end..]);
    let theirs = dir.path().join("theirs.pmtiles");
    std::fs::write(&theirs, &foreign).unwrap();

    // Decode both; the foreign layout must yield exactly the same tiles.
    let decode = |input: &Path, name: &str| {
        let out = dir.path().join(name);
        decode_pmtiles(input, &out, &DecodeOptions::default()).unwrap()
    };
    let ours_report = decode(&ours, "ours.parquet");
    let theirs_report = decode(&theirs, "theirs.parquet");
    assert_eq!(ours_report.tiles_read, u64::from(N));
    assert_eq!(theirs_report.tiles_read, ours_report.tiles_read);
    assert_eq!(theirs_report.features_written, ours_report.features_written);
    assert_eq!(theirs_report.features_written, u64::from(N));
}

/// The raw `geo` footer value of a GeoParquet file, exactly as written.
fn geo_footer_json(path: &Path) -> String {
    let file = std::fs::File::open(path).unwrap();
    let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
    builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .expect("decoded output must carry footer key-value metadata")
        .iter()
        .find(|kv| kv.key == "geo")
        .expect("decoded output must carry a `geo` footer key")
        .value
        .clone()
        .expect("`geo` footer key must have a value")
}

/// #508 (review S2): the decode writer's footer must be byte-stable.
///
/// `GeoParquetColumnMetadata::geometry_types` is a `HashSet`; its default
/// `Serialize` emits the array in hash-iteration order, which varies between
/// sets even within one process. `overview::writer` routes around that
/// (`geo_metadata_json_deterministic`), but `decode` writes its own
/// GeoParquet through the encoder's `into_keyvalue()` — and a decoded
/// archive is the COMMON mixed-geometry case (points, lines and polygons all
/// land in one output file), so the flip is not an edge case here.
///
/// Four decodes of the same archive in one process: a single differing
/// footer is a failure.
#[test]
fn decode_geo_footer_is_byte_stable_across_runs() {
    let dir = tempfile::tempdir().unwrap();
    let pmtiles = build_archive(&dir, None);

    let mut baseline: Option<String> = None;
    for run in 0..4 {
        let out = dir.path().join(format!("decoded-{run}.parquet"));
        decode_pmtiles(&pmtiles, &out, &DecodeOptions::default()).unwrap();
        let geo = geo_footer_json(&out);
        // Guard against a vacuous pass: the fixture must actually produce
        // more than one geometry type, or there is no order to get wrong.
        let parsed: serde_json::Value = serde_json::from_str(&geo).unwrap();
        let types = parsed["columns"]["geometry"]["geometry_types"]
            .as_array()
            .expect("geometry_types array");
        assert!(
            types.len() > 1,
            "fixture must decode to MIXED geometry types for this test to mean \
             anything; got {types:?}"
        );
        match &baseline {
            None => baseline = Some(geo),
            Some(first) => assert_eq!(
                first, &geo,
                "run {run}: the decoded `geo` footer differs from run 0 — a \
                 HashSet iteration order is leaking into the output bytes"
            ),
        }
    }
}
