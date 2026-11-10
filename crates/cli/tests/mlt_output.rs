//! `--tile-format mlt` (#823): one run tiled as MVT and as MLT, every tile of
//! both archives decoded independently (prost-free: mlt-core's MVT reader and
//! its MLT decoder) and compared feature by feature, plus the readers
//! (`stats --layers`, `decode`), `merge` and `pyramid` on MLT archives.
#![cfg(feature = "mlt")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use tylertoo_core::internal::archive_index::ArchiveIndex;
use tylertoo_core::internal::compression::decompress_capped;
use tylertoo_core::internal::mlt_core::{self, Decoder, Parser, PropValue, TileLayer};
use tylertoo_core::{Compression, TileType};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

fn tylertoo(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tylertoo"))
        .args(args)
        .output()
        .expect("run tylertoo")
}

fn ok(args: &[&str]) {
    let out = tylertoo(args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn tile(dir: &Path, input: &Path, name: &str, extra: &[&str]) -> PathBuf {
    let out = dir.join(name);
    let mut args = vec!["tiles", input.to_str().unwrap(), out.to_str().unwrap()];
    args.extend_from_slice(extra);
    ok(&args);
    out
}

/// Every tile of `path`, uncompressed, by tile id.
fn tiles(path: &Path) -> (TileType, BTreeMap<u64, Vec<u8>>) {
    let index = ArchiveIndex::open(path).unwrap();
    let tiles = index
        .tiles()
        .map(|t| {
            let t = t.unwrap();
            let raw = index.read_range(t.range).unwrap();
            (
                t.id,
                decompress_capped(&raw, Compression::Gzip, 1 << 30).unwrap(),
            )
        })
        .collect();
    (index.header().tile_type, tiles)
}

fn mlt_layers(bytes: &[u8]) -> Vec<TileLayer> {
    let mut dec = Decoder::default();
    Parser::default()
        .parse_layers(bytes)
        .unwrap()
        .into_iter()
        .filter_map(|l| l.into_tile(&mut dec).unwrap())
        .collect()
}

/// A value under #823's typing rules: an integer and a float are equal when
/// the float is that integer exactly.
#[derive(Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
}

fn value(v: &PropValue) -> Value {
    #[expect(clippy::cast_precision_loss, reason = "test values are small")]
    match v.clone() {
        PropValue::Bool(Some(b)) => Value::Bool(b),
        PropValue::I8(Some(n)) => Value::Num(n.into()),
        PropValue::U8(Some(n)) => Value::Num(n.into()),
        PropValue::I32(Some(n)) => Value::Num(n.into()),
        PropValue::U32(Some(n)) => Value::Num(n.into()),
        PropValue::I64(Some(n)) => Value::Num(n as f64),
        PropValue::U64(Some(n)) => Value::Num(n as f64),
        PropValue::F32(Some(n)) => Value::Num(n.into()),
        PropValue::F64(Some(n)) => Value::Num(n),
        PropValue::Str(Some(s)) => Value::Str(s.to_string()),
        _ => Value::Null,
    }
}

/// Each feature as (id, geometry, {name: value}), nulls left out.
fn rows(layer: &TileLayer) -> Vec<(Option<u64>, String, BTreeMap<String, Value>)> {
    layer
        .features()
        .iter()
        .map(|f| {
            let props = layer
                .property_names()
                .iter()
                .zip(f.properties())
                .map(|(n, v)| (n.clone(), value(v)))
                .filter(|(_, v)| *v != Value::Null)
                .collect();
            (f.id(), format!("{:?}", f.geometry()), props)
        })
        .collect()
}

/// The two archives hold the same tiles, layers, features, ids, geometry
/// and values; returns (tiles, features).
fn assert_same_content(mvt: &Path, mlt: &Path) -> (usize, usize) {
    let (mvt_type, mvt_tiles) = tiles(mvt);
    let (mlt_type, mlt_tiles) = tiles(mlt);
    assert_eq!((mvt_type, mlt_type), (TileType::Mvt, TileType::Mlt));
    assert_eq!(
        mvt_tiles.keys().collect::<Vec<_>>(),
        mlt_tiles.keys().collect::<Vec<_>>()
    );
    let mut features = 0;
    for (id, mvt_bytes) in &mvt_tiles {
        let want = mlt_core::mvt::mvt_to_tile_layers(mvt_bytes).unwrap();
        let got = mlt_layers(&mlt_tiles[id]);
        assert_eq!(got.len(), want.len(), "tile {id}: layers");
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(g.name(), w.name(), "tile {id}");
            assert_eq!(g.extent(), w.extent(), "tile {id}");
            assert_eq!(rows(g), rows(w), "tile {id}, layer {}", w.name());
            features += w.feature_count();
        }
    }
    (mvt_tiles.len(), features)
}

#[test]
fn mlt_archive_matches_mvt_archive_tile_for_tile() {
    for name in ["road-detections.parquet", "open-buildings.parquet"] {
        let Some(input) = fixture::realdata(name) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let zooms = ["--min-zoom", "0", "--max-zoom", "9"];
        let mvt = tile(dir.path(), &input, "mvt.pmtiles", &zooms);
        let mlt = tile(
            dir.path(),
            &input,
            "mlt.pmtiles",
            &[&zooms[..], &["--tile-format", "mlt"]].concat(),
        );
        let (tiles, features) = assert_same_content(&mvt, &mlt);
        assert!(
            tiles >= 5 && features > 100,
            "{name}: {tiles} tiles, {features} features"
        );

        let format =
            |p: &Path| ArchiveIndex::open(p).unwrap().metadata_json().unwrap()["format"].clone();
        assert_eq!((format(&mvt), format(&mlt)), ("pbf".into(), "mlt".into()));

        // `stats --layers` counts the same features per zoom and layer.
        let counts = |p: &Path| {
            let index = ArchiveIndex::open(p).unwrap();
            let stats = tylertoo_core::internal::stats::compute_stats(&index, 0, true).unwrap();
            stats
                .per_layer
                .unwrap()
                .into_iter()
                .map(|l| (l.z, l.layer, l.feature_count, l.geometry_types))
                .collect::<Vec<_>>()
        };
        assert_eq!(counts(&mlt), counts(&mvt), "{name}");
        ok(&["stats", mlt.to_str().unwrap(), "--layers"]);

        // `decode` writes the same rows.
        let decode = |p: &Path, out: &str| {
            tylertoo_core::decode_pmtiles(p, dir.path().join(out), &Default::default()).unwrap()
        };
        let (a, b) = (decode(&mvt, "mvt.parquet"), decode(&mlt, "mlt.parquet"));
        assert_eq!(
            (a.tiles_read, a.features_written, a.layers),
            (b.tiles_read, b.features_written, b.layers)
        );
    }
}

#[test]
fn mlt_shards_merge_and_mixed_formats_are_refused() {
    let Some(input) = fixture::realdata("road-detections.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mlt = |name, zooms: [&str; 2]| {
        let args = [
            "--tile-format",
            "mlt",
            "--min-zoom",
            zooms[0],
            "--max-zoom",
            zooms[1],
        ];
        tile(dir.path(), &input, name, &args)
    };
    let (low, high) = (mlt("lo.pmtiles", ["0", "3"]), mlt("hi.pmtiles", ["4", "9"]));
    let mvt = tile(dir.path(), &input, "mvt.pmtiles", &["--min-zoom", "4"]);
    let merged = dir.path().join("merged.pmtiles");
    ok(&[
        "merge",
        merged.to_str().unwrap(),
        low.to_str().unwrap(),
        high.to_str().unwrap(),
    ]);
    assert_eq!(tiles(&merged).0, TileType::Mlt);

    let mixed = dir.path().join("mixed.pmtiles");
    let out = tylertoo(&[
        "merge",
        mixed.to_str().unwrap(),
        low.to_str().unwrap(),
        mvt.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("Mvt") && err.contains("Mlt") && err.contains("--tile-format"),
        "{err}"
    );
}

#[test]
fn pyramid_writes_mlt_bands_and_takes_a_memory_profile() {
    let Some(input) = fixture::realdata("road-detections.parquet") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let pyramid = |name: &str, extra: &[&str]| {
        let out = dir.path().join(name);
        let lo = format!("0-4:{}:low", input.display());
        let hi = format!("5-8:{}:high", input.display());
        let mut args = vec![
            "pyramid",
            out.to_str().unwrap(),
            "--band",
            &lo,
            "--band",
            &hi,
        ];
        args.extend_from_slice(extra);
        ok(&args);
        std::fs::read(out).unwrap()
    };
    let mlt = pyramid("mlt.pmtiles", &["--tile-format", "mlt"]);
    assert_eq!(TileType::from_code(mlt[99]), Some(TileType::Mlt));
    let path = dir.path().join("mlt.pmtiles");
    let (_, tiles) = tiles(&path);
    let names: std::collections::BTreeSet<String> = tiles
        .values()
        .flat_map(|t| mlt_layers(t).into_iter().map(|l| l.name().to_owned()))
        .collect();
    assert_eq!(names, ["high".to_owned(), "low".to_owned()].into());

    // #801: profiles change memory, not bytes.
    let auto = pyramid("auto.pmtiles", &[]);
    assert_eq!(pyramid("bounded.pmtiles", &["--profile", "bounded"]), auto);
    assert_eq!(pyramid("speed.pmtiles", &["--profile", "speed"]), auto);

    // A pre-tiled MVT band is refused under --tile-format mlt.
    let band = format!("0-4:{}:low", dir.path().join("auto.pmtiles").display());
    let out = tylertoo(&[
        "pyramid",
        dir.path().join("x.pmtiles").to_str().unwrap(),
        "--band",
        &band,
        "--tile-format",
        "mlt",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("--tile-format is mlt"));
}
