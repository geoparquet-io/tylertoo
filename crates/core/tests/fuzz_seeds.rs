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
    for (name, bytes) in seeds(target) {
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
    replay("plan_file", &[], tylertoo_core::overview::fuzz_convert_plan);
}

#[test]
fn decode_archive_seeds() {
    replay(
        "decode_archive",
        &[],
        tylertoo_core::decode::fuzz_decode_archive,
    );
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
    replay(
        "parquet_footer",
        &[],
        tylertoo_core::overview::check::fuzz_parquet_footer,
    );
}

#[test]
fn overview_file_seeds() {
    replay(
        "overview_file",
        &[],
        tylertoo_core::overview::reader::fuzz_overview_file,
    );
}

#[cfg(feature = "remote")]
#[test]
fn remote_footer_seeds() {
    replay(
        "remote_footer",
        &[],
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

#[test]
fn mvt_decode_seeds() {
    replay("mvt_decode", &[], |b| {
        let (&first, tile) = b.split_first().expect("a zoom byte");
        let z = first & 31;
        let max = if z == 0 { 0 } else { (1u32 << z) - 1 };
        tylertoo_core::decode::fuzz_decode_tile(z, max, max, tile)
    });
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
