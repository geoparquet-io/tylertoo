//! Replay every committed fuzz seed through its target's hook (#633).
//!
//! The nightly fuzz run (`.github/workflows/fuzz.yml`) is the only thing that
//! builds `fuzz/`, so without this a hook could stop compiling, or a seed
//! could stop reaching the code it was cut for, and nobody would hear until
//! the next night. CI runs the test tiers with `--all-features`, which turns
//! on `fuzzing`, so this runs on every PR.
//!
//! Each seed must come back without a panic. Seeds cut from real inputs must
//! also come back `Ok`: a seed the hook rejects early only fuzzes the
//! rejection. The few seeds that are hostile on purpose are listed by name.
#![cfg(feature = "fuzzing")]

use std::fmt::Debug;
use std::path::PathBuf;

/// `(file name, bytes)` of every `seed-*` in `fuzz/corpus/<target>/`.
fn seeds(target: &str) -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fuzz/corpus")
        .join(target);
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("seed-"))
        })
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            (name, std::fs::read(&p).unwrap())
        })
        .collect();
    out.sort();
    assert!(!out.is_empty(), "{target} has no seeds");
    out
}

/// Run `hook` on every seed of `target`; every seed not named in `hostile`
/// must come back `Ok`.
fn replay<T: Debug, E: Debug>(
    target: &str,
    hostile: &[&str],
    hook: impl Fn(&[u8]) -> Result<T, E>,
) {
    let all = seeds(target);
    for name in hostile {
        assert!(
            all.iter().any(|(n, _)| n == name),
            "{target}: hostile seed {name} is not in the corpus"
        );
    }
    for (name, bytes) in all {
        let result = hook(&bytes);
        if !hostile.contains(&name.as_str()) {
            assert!(result.is_ok(), "{target}/{name}: {result:?}");
        }
    }
}

fn utf8(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("a string seed is UTF-8")
}

#[test]
fn geo_metadata_seeds() {
    // EPSG:32633 is a real CRS tylertoo refuses (UTM), on purpose.
    replay("geo_metadata", &["seed-utm.json"], |b| {
        tylertoo_core::overview::convert::fuzz_geo_metadata(utf8(b))
    });
}

#[test]
fn plan_file_seeds() {
    // #684's crash input: a crafted IPC buffer offset, refused (#779).
    replay(
        "plan_file",
        &["seed-issue-684-ipc-buffer.arrow"],
        tylertoo_core::overview::fuzz_convert_plan,
    );
}

/// `decode_pmtiles` on `archive`, written to a temp file.
fn decode_report(archive: &[u8]) -> tylertoo_core::decode::DecodeReport {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("seed.pmtiles");
    std::fs::write(&input, archive).unwrap();
    tylertoo_core::decode::decode_pmtiles(
        &input,
        dir.path().join("seed.parquet"),
        &tylertoo_core::decode::DecodeOptions::default(),
    )
    .unwrap()
}

#[test]
fn decode_archive_seeds() {
    replay(
        "decode_archive",
        &[],
        tylertoo_core::decode::fuzz_decode_archive,
    );
    // The hook must walk the archive `decode` walks: a hook that returned
    // early would still come back `Ok` and fuzz nothing.
    for (name, bytes) in seeds("decode_archive") {
        let (tiles, features) = tylertoo_core::decode::fuzz_decode_archive(&bytes).unwrap();
        let report = decode_report(&bytes);
        assert!(tiles > 0, "{name}");
        assert_eq!(tiles as u64, report.tiles_read, "{name}");
        assert_eq!(
            features as u64,
            report.features_written + report.features_skipped,
            "{name}"
        );
    }
}

#[test]
fn archive_index_seeds() {
    replay(
        "archive_index",
        &[],
        tylertoo_core::fuzz::fuzz_archive_index,
    );
}

#[test]
fn layer_zooms_seeds() {
    replay("layer_zooms", &[], tylertoo_core::pyramid::fuzz_layer_zooms);
}

#[test]
fn parquet_footer_seeds() {
    // #690's crash input: a schema child count the footer pre-check
    // refuses before parquet-rs reserves memory for it (#779).
    replay(
        "parquet_footer",
        &["seed-issue-690-num-children.thrift"],
        tylertoo_core::overview::check::fuzz_parquet_footer,
    );
}

#[test]
fn overview_file_seeds() {
    // #683's crash input: a crafted ARROW:schema key, refused (#779).
    replay(
        "overview_file",
        &["seed-issue-683-arrow-schema.parquet"],
        tylertoo_core::overview::reader::fuzz_overview_file,
    );
}

#[cfg(feature = "remote")]
#[test]
fn remote_footer_seeds() {
    // #683's and #687's crash inputs: a crafted ARROW:schema key and a
    // crafted page's definition levels, refused (#779).
    replay(
        "remote_footer",
        &[
            "seed-issue-683-arrow-schema.parquet",
            "seed-issue-687-def-levels.parquet",
        ],
        tylertoo_core::input::fuzz_remote_footer,
    );
}

#[test]
fn shard_plan_seeds() {
    replay("shard_plan", &[], tylertoo_core::fuzz::fuzz_shard_plan);
}

#[test]
fn decompress_seeds() {
    replay("decompress", &[], tylertoo_core::fuzz::fuzz_decompress);
}

#[test]
fn cli_args_seeds() {
    // No `Result` to check: each parser either accepts the string or not.
    // Panicking is the failure, as #681's input did before its fix.
    replay("cli_args", &[], |b| {
        tylertoo_core::fuzz::fuzz_cli_args(utf8(b));
        Ok::<(), ()>(())
    });
}

#[test]
fn band_archive_seeds() {
    replay(
        "band_archive",
        &[],
        tylertoo_core::pyramid::fuzz_band_archive,
    );
}

/// The tile coordinates `mvt_decode` gives a seed: its first byte picks
/// the zoom, and the tile is that zoom's south-east corner.
fn mvt_seed(b: &[u8]) -> (u8, u32, &[u8]) {
    let (&first, tile) = b.split_first().expect("a zoom byte");
    let z = first & 31;
    let max = if z == 0 { 0 } else { (1u32 << z) - 1 };
    (z, max, tile)
}

#[test]
fn mvt_decode_seeds() {
    replay("mvt_decode", &[], |b| {
        let (z, max, tile) = mvt_seed(b);
        tylertoo_core::decode::fuzz_decode_tile(z, max, max, tile)
    });
    // The hook's counts are `decode`'s for the same tile in an archive.
    for (name, bytes) in seeds("mvt_decode") {
        let (z, max, tile) = mvt_seed(&bytes);
        let (features, with_geometry) =
            tylertoo_core::decode::fuzz_decode_tile(z, max, max, tile).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("tile.pmtiles");
        let mut writer =
            tylertoo_core::StreamingPmtilesWriter::new(tylertoo_core::Compression::None).unwrap();
        writer.add_tile(z, max, max, tile).unwrap();
        writer.finalize(&archive).unwrap();
        let report = decode_report(&std::fs::read(&archive).unwrap());
        assert!(features > 0, "{name}");
        assert_eq!(with_geometry as u64, report.features_written, "{name}");
        assert_eq!(
            (features - with_geometry) as u64,
            report.features_skipped,
            "{name}"
        );
    }
}

#[test]
fn geoparquet_wkb_seeds() {
    // #632's crash inputs: rejected, not decoded.
    replay(
        "geoparquet_wkb",
        &[
            "seed-issue-632-deep-nesting",
            "seed-issue-632-multipolygon-count",
            "seed-multipoint-srid-member",
        ],
        tylertoo_core::batch_processor::fuzz_geoparquet_wkb,
    );
}
