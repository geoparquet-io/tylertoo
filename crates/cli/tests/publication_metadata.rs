//! `--name`, `--description` and `--attribution` reach the archive from every
//! command that writes one: `tiles` (both invocation forms, and the empty
//! shard of a sharded build), `export-pmtiles`, `pyramid` and `merge`.
//!
//! Real processes, so clap parsing and the bare-form rewrite are part of what
//! is tested. Every input is the tracked grid fixture: none of this skips
//! without the downloaded real-data fixtures.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{json, Value};
use tylertoo_core::archive_index::ArchiveIndex;

/// HTML, Unicode, quotes, a backslash and a newline in one value.
const HOSTILE: &str =
    "© NPS — <a href=\"https://www.nps.gov/\">Parks & Trails</a>\\data\nSecond line";

fn tylertoo_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tylertoo")
}

fn grid() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/sharding-grid.parquet")
}

fn run(args: &[&str]) -> (bool, String) {
    let out = Command::new(tylertoo_bin())
        .args(args)
        .output()
        .expect("run tylertoo");
    let merged = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), merged)
}

fn s(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// The archive's `name`, `description` and `attribution` keys, `None` when
/// absent.
fn publication(path: &Path) -> [Option<Value>; 3] {
    let meta = ArchiveIndex::open(path)
        .unwrap()
        .metadata_json()
        .expect("metadata is JSON");
    ["name", "description", "attribution"].map(|k| meta.get(k).cloned())
}

/// Every tile id's stored bytes.
fn tiles(path: &Path) -> BTreeMap<u64, Vec<u8>> {
    let idx = ArchiveIndex::open(path).unwrap();
    idx.tiles()
        .map(|t| {
            let t = t.unwrap();
            (t.id, idx.read_range(t.range).unwrap())
        })
        .collect()
}

/// `tiles` the grid over `zooms` into `out`, with `extra` flags.
fn tile_grid(out: &Path, zooms: (&str, &str), extra: &[&str]) {
    let input = grid();
    let mut argv = vec![
        "tiles",
        s(&input),
        s(out),
        "--min-zoom",
        zooms.0,
        "--max-zoom",
        zooms.1,
        "--layer-name",
        "grid",
    ];
    argv.extend_from_slice(extra);
    let (ok, log) = run(&argv);
    assert!(ok, "tiles {extra:?}: {log}");
}

#[test]
fn tiles_writes_exact_values_and_leaves_tiles_alone() {
    let dir = tempfile::tempdir().unwrap();
    let (plain, published) = (dir.path().join("p.pmtiles"), dir.path().join("m.pmtiles"));
    tile_grid(&plain, ("0", "3"), &[]);
    tile_grid(
        &published,
        ("0", "3"),
        &[
            "--name",
            "Park Tiles 4",
            "--description",
            "",
            "--attribution",
            HOSTILE,
        ],
    );
    assert_eq!(publication(&plain), [None, None, None]);
    assert_eq!(
        publication(&published),
        [
            Some(json!("Park Tiles 4")),
            Some(json!("")),
            Some(json!(HOSTILE))
        ]
    );
    assert_eq!(tiles(&plain), tiles(&published));
}

/// The bare `INPUT OUTPUT` form forwards the flags like `tiles` does, even a
/// value that names a subcommand; a value that looks like a flag takes `=`.
#[test]
fn bare_and_explicit_forms_forward_the_same_values() {
    let dir = tempfile::tempdir().unwrap();
    let cases: [(&str, Vec<String>); 3] = [
        ("bare", vec!["--name".into(), "tiles".into()]),
        ("explicit", vec!["--name".into(), "tiles".into()]),
        ("equals", vec!["--name=--help".into()]),
    ];
    for (label, flags) in cases {
        let out = dir.path().join(format!("{label}.pmtiles"));
        let mut argv: Vec<String> = Vec::new();
        if label == "explicit" {
            // Flags before the positionals, which needs the subcommand.
            argv.push("tiles".into());
            argv.extend(flags.iter().cloned());
            argv.extend([s(&grid()).to_string(), s(&out).to_string()]);
        } else {
            argv.extend([s(&grid()).to_string(), s(&out).to_string()]);
            argv.extend(flags.iter().cloned());
        }
        argv.extend(["--max-zoom".into(), "2".into()]);
        let (ok, log) = run(&argv.iter().map(String::as_str).collect::<Vec<_>>());
        assert!(ok, "{label}: {log}");
        let want = if label == "equals" { "--help" } else { "tiles" };
        assert_eq!(publication(&out)[0], Some(json!(want)), "{label}");
    }
}

#[test]
fn export_pmtiles_writes_the_values() {
    let dir = tempfile::tempdir().unwrap();
    let overview = dir.path().join("ov.parquet");
    let first = dir.path().join("first.pmtiles");
    tile_grid(&first, ("0", "2"), &["--keep-overview", s(&overview)]);
    let out = dir.path().join("out.pmtiles");
    let (ok, log) = run(&[
        "export-pmtiles",
        s(&overview),
        s(&out),
        "--layer-name",
        "grid",
        "--description",
        "Grid cells",
        "--attribution",
        "",
    ]);
    assert!(ok, "{log}");
    assert_eq!(
        publication(&out),
        [None, Some(json!("Grid cells")), Some(json!(""))]
    );
}

/// Two archives over disjoint zooms of the same layer, credited differently.
fn two_bands(dir: &Path, second_name: &str) -> (PathBuf, PathBuf) {
    let (coarse, fine) = (dir.join("coarse.pmtiles"), dir.join("fine.pmtiles"));
    tile_grid(
        &coarse,
        ("0", "1"),
        &["--name", "Grid", "--attribution", "USGS"],
    );
    tile_grid(
        &fine,
        ("2", "3"),
        &["--name", second_name, "--attribution", "NPS"],
    );
    (coarse, fine)
}

#[test]
fn pyramid_inherits_and_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let (coarse, fine) = two_bands(dir.path(), "Grid");
    let bands = |out: &Path, extra: &[&str]| -> (bool, String) {
        let (b0, b1) = (
            format!("0-1:{}:grid", s(&coarse)),
            format!("2-3:{}:grid", s(&fine)),
        );
        let mut argv = vec!["pyramid", s(out), "--band", &b0, "--band", &b1];
        argv.extend_from_slice(extra);
        run(&argv)
    };
    let out = dir.path().join("out.pmtiles");
    let (ok, log) = bands(&out, &["--description", "All zooms"]);
    assert!(ok, "{log}");
    assert_eq!(
        publication(&out),
        [
            Some(json!("Grid")),
            Some(json!("All zooms")),
            Some(json!("NPS | USGS"))
        ]
    );
    let out = dir.path().join("override.pmtiles");
    let (ok, log) = bands(&out, &["--name", "Pyramid", "--attribution", HOSTILE]);
    assert!(ok, "{log}");
    assert_eq!(
        publication(&out),
        [Some(json!("Pyramid")), None, Some(json!(HOSTILE))]
    );
}

#[test]
fn merge_inherits_and_overrides() {
    let dir = tempfile::tempdir().unwrap();
    let (coarse, fine) = two_bands(dir.path(), "Grid");
    let out = dir.path().join("out.pmtiles");
    let (ok, log) = run(&["merge", s(&out), s(&fine), s(&coarse)]);
    assert!(ok, "{log}");
    assert!(!log.contains("merging a single archive"), "{log}");
    assert_eq!(
        publication(&out),
        [Some(json!("Grid")), None, Some(json!("NPS | USGS"))]
    );
    let out = dir.path().join("cleared.pmtiles");
    let (ok, log) = run(&["merge", s(&out), s(&coarse), s(&fine), "--attribution", ""]);
    assert!(ok, "{log}");
    assert_eq!(
        publication(&out),
        [Some(json!("Grid")), None, Some(json!(""))]
    );

    let copy = dir.path().join("copy.pmtiles");
    let (ok, log) = run(&["merge", s(&copy), s(&coarse)]);
    assert!(ok, "{log}");
    assert!(
        log.contains("merging a single archive just rewrites it"),
        "{log}"
    );
    assert_eq!(tiles(&copy), tiles(&coarse));
}

/// Differing inherited names are left out, on both composing commands, and
/// stdout says so next to the values that were written; `--name` sets one.
#[test]
fn disagreeing_names_are_left_out_and_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (coarse, fine) = two_bands(dir.path(), "Other grid");
    let (b0, b1) = (
        format!("0-1:{}:grid", s(&coarse)),
        format!("2-3:{}:grid", s(&fine)),
    );
    let (merged, pyramid) = (
        dir.path().join("merged.pmtiles"),
        dir.path().join("pyramid.pmtiles"),
    );
    for (argv, out) in [
        (vec!["merge", s(&merged), s(&coarse), s(&fine)], &merged),
        (
            vec!["pyramid", s(&pyramid), "--band", &b0, "--band", &b1],
            &pyramid,
        ),
    ] {
        let (ok, log) = run(&argv);
        assert!(ok, "{argv:?}: {log}");
        assert_eq!(
            publication(out),
            [None, None, Some(json!("NPS | USGS"))],
            "{argv:?}"
        );
        assert!(
            log.contains(r#"attribution: "NPS | USGS""#),
            "{argv:?}: {log}"
        );
        assert!(
            log.contains("different names, so the output has none; set --name"),
            "{argv:?}: {log}"
        );
    }
    let out = dir.path().join("named.pmtiles");
    let (ok, log) = run(&["merge", s(&out), s(&coarse), s(&fine), "--name", "Both"]);
    assert!(ok, "{log}");
    assert_eq!(publication(&out)[0], Some(json!("Both")));
    assert!(!log.contains("different names"), "{log}");
}

/// A sharded build: every job gets the same flags, the shards that own no
/// rows write empty archives that still carry them, and the merge keeps them.
#[test]
fn a_sharded_fleet_carries_the_values_through_empty_shards() {
    let dir = tempfile::tempdir().unwrap();
    let plan = dir.path().join("shards.json");
    let (ok, log) = run(&[
        "shard-plan",
        s(&grid()),
        "--shards",
        "4",
        "--pivot",
        "3",
        "-o",
        s(&plan),
    ]);
    assert!(ok, "{log}");
    let convert_plan = dir.path().join("convert.plan");
    let input = grid();
    let job = |out: &Path, extra: &[&str]| -> String {
        let report = out.with_extension("json");
        std::fs::write(&report, "{\"stale\":true}").unwrap();
        let mut argv = vec![
            "tiles",
            s(&input),
            s(out),
            "--min-zoom",
            "0",
            "--max-zoom",
            "4",
            // One corner of the world, so most shards own nothing.
            "--bbox=-175,-70,-120,-20",
            "--report",
            s(&report),
            "--no-coalesce-lines",
            "--shard-plan",
            s(&plan),
            "--name",
            "Grid",
            "--attribution",
            "Grid survey",
        ];
        argv.extend_from_slice(extra);
        let (ok, log) = run(&argv);
        assert!(ok, "{extra:?}: {log}");
        let report: Value = serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
        assert!(
            report.get("stale").is_none(),
            "success must replace a previous report"
        );
        if log.contains("owns no input rows") {
            assert_eq!(report["total_tiles"], 0);
            assert!(report["diagnostics"].is_array());
        }
        log
    };
    let coarse = dir.path().join("coarse.pmtiles");
    job(
        &coarse,
        &["--shard", "coarse", "--save-plan", s(&convert_plan)],
    );
    let mut archives = vec![coarse];
    let mut empties = 0;
    for i in 0..4 {
        let shard = dir.path().join(format!("shard-{i}.pmtiles"));
        let selector = format!("{i}/4");
        let log = job(&shard, &["--shard", &selector, "--plan", s(&convert_plan)]);
        empties += usize::from(log.contains("owns no input rows"));
        archives.push(shard);
    }
    assert!(empties > 0, "the bbox must leave a shard empty");
    let want = [Some(json!("Grid")), None, Some(json!("Grid survey"))];
    for a in &archives {
        assert_eq!(publication(a), want, "{}", a.display());
    }
    let merged = dir.path().join("merged.pmtiles");
    let mut argv = vec!["merge", s(&merged)];
    argv.extend(archives.iter().map(|p| s(p)));
    let (ok, log) = run(&argv);
    assert!(ok, "{log}");
    assert_eq!(publication(&merged), want);
}
