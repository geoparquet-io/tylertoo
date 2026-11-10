use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::CommandFactory;
use tylertoo_core::internal::overview::config::cli::{ExportPmtilesArgs, PyramidArgs};

fn grid() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/sharding-grid.parquet")
}

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tylertoo"))
        .args(args)
        .output()
        .unwrap()
}

fn success(args: &[&str]) -> Output {
    let out = run(args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn s(path: &Path) -> &str {
    path.to_str().unwrap()
}

#[test]
fn validate_good_and_tampered_overview() {
    let dir = tempfile::tempdir().unwrap();
    let overview = dir.path().join("overview.parquet");
    success(&[
        "overview",
        s(&grid()),
        s(&overview),
        "--min-zoom",
        "0",
        "--max-zoom",
        "1",
    ]);
    let valid = run(&["validate", s(&overview)]);
    assert_eq!(valid.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&valid.stdout).contains("valid overview file"));

    let mut bytes = std::fs::read(&overview).unwrap();
    let key = b"geo:overviews";
    let pos = bytes.windows(key.len()).position(|w| w == key).unwrap();
    bytes[pos + key.len() - 1] = b'X';
    let bad = dir.path().join("tampered.parquet");
    std::fs::write(&bad, bytes).unwrap();
    let invalid = run(&["validate", s(&bad)]);
    assert_eq!(invalid.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&invalid.stdout).contains("[FAIL]"));
    assert!(String::from_utf8_lossy(&invalid.stderr).contains("check(s) failed"));
}

#[test]
fn decode_archive_to_parquet() {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("tiles.pmtiles");
    let decoded = dir.path().join("decoded.parquet");
    success(&[
        "tiles",
        s(&grid()),
        s(&archive),
        "--min-zoom",
        "0",
        "--max-zoom",
        "0",
    ]);
    success(&["decode", s(&archive), s(&decoded)]);
    let reader = tylertoo_io::open_parquet(std::fs::File::open(decoded).unwrap()).unwrap();
    let rows = reader.metadata().file_metadata().num_rows();
    assert_eq!(rows, 1440);
}

#[test]
fn pyramid_builds_two_bands() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("pyramid.pmtiles");
    let b0 = format!("0-0:{}:grid", grid().display());
    let b1 = format!("1-1:{}:grid", grid().display());
    let result = success(&["pyramid", s(&out), "--band", &b0, "--band", &b1]);
    assert!(String::from_utf8_lossy(&result.stdout).contains("Built 2 band(s)"));
    let index = tylertoo_core::internal::archive_index::ArchiveIndex::open(&out).unwrap();
    // Bands are z0 and z1: the header and the zooms that really hold tiles
    // must both span exactly 0..=1, so a dropped band fails.
    assert_eq!((index.header().min_zoom, index.header().max_zoom), (0, 1));
    assert_eq!(index.actual_zoom_range().unwrap(), Some((0, 1)));
}

/// #801: `pyramid --profile` runs every band under that profile, and a
/// profile changes memory, never the archive.
#[test]
fn pyramid_profiles_write_identical_archives() {
    let dir = tempfile::tempdir().unwrap();
    let b0 = format!("0-0:{}:grid", grid().display());
    let b1 = format!("1-3:{}:grid", grid().display());
    let archives: Vec<Vec<u8>> = ["auto", "speed", "bounded"]
        .into_iter()
        .map(|profile| {
            let out = dir.path().join(format!("{profile}.pmtiles"));
            success(&[
                "pyramid",
                s(&out),
                "--band",
                &b0,
                "--band",
                &b1,
                "--profile",
                profile,
            ]);
            std::fs::read(out).unwrap()
        })
        .collect();
    assert_eq!(archives[0], archives[1]);
    assert_eq!(archives[0], archives[2]);
}

#[test]
fn pyramid_has_parity_with_export_flags() {
    fn longs(cmd: clap::Command) -> BTreeSet<String> {
        cmd.get_arguments()
            .filter(|a| !a.is_hide_set())
            .flat_map(|a| {
                a.get_long()
                    .into_iter()
                    .chain(a.get_visible_aliases().into_iter().flatten())
            })
            .map(str::to_owned)
            .collect()
    }
    let export = longs(ExportPmtilesArgs::command());
    let pyramid = longs(PyramidArgs::command());
    // Band specifications own layer names and zoom bounds. The rest are
    // exempt for the reasons next to each flag.
    let exemptions: BTreeSet<_> = [
        "layer-name",
        "min-zoom",
        "zoom-ceiling",
        // Shard option: build_pyramid_validated clones the export options per
        // band, so one range would hit every band's intermediate archive.
        "tile-range",
        // Redundant with pyramid's --work-dir, which band export spill falls
        // back to (pyramid.rs, #427).
        "spill-dir",
        // #803: refused; a variable-depth band's missing tiles mean "draw
        // the stored ancestor", which the band merge cannot keep.
        "generate-variable-depth-tile-pyramid",
        // #822: bands tile verbatim and take no convert tuning, so a
        // tippecanoe preset could set only half of itself.
        "preset",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(exemptions.len(), 7);
    let missing: BTreeSet<_> = export.difference(&pyramid).cloned().collect();
    assert_eq!(missing, exemptions);
}
