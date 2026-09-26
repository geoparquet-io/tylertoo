//! #558: the golden tile guard — a real-data build whose *tile bytes* are
//! pinned, so an output-changing dependency bump cannot pass CI green.
//!
//! # Why this exists
//!
//! The structural convert guard (`benchmarks/overview/ci_guard.py`, run by
//! the `Convert regression guard` job in `.github/workflows/bench.yml`) green-lit
//! the `i_overlay` 8.1.2 → 9.0.0 / `i_float` 4 → 5 bump in 5c123fa — our own
//! direct dependency, the engine behind the `clip_geometry` boundary-bridge
//! fallback and the #383 quantization repair, not the `i_overlay` 4.5.2 that
//! `geo` vendors for `BooleanOps`. It changed real tile output: on a Brazil
//! coarse build, 18 of 588 tiles moved
//! (±1-MVT-unit coordinate shifts in clipped boundaries across z5–z8, and one
//! small polygon flipping across its collapse threshold).
//!
//! It passed for two structural reasons, both of them by construction:
//!
//! 1. **It stops at pass 1.** `ci_guard.py` runs `tylertoo overview` and
//!    compares the convert report. Clipping happens in *export*, against tile
//!    windows, and never runs at all in what that guard measures.
//! 2. **It compares counts, not coordinates.** Its signature is per-level
//!    feature/vertex counts plus totals. A clipper that returns the same
//!    vertices at slightly different coordinates is invisible to it — which is
//!    exactly what a boolean-ops engine change looks like.
//!
//! This test closes both gaps: it runs the production chain
//! (`convert_to_overviews` → `export_pmtiles`, per CLAUDE.md pitfall 6) over a
//! clipper-stressing real-data fixture and compares a **per-tile digest of the
//! decompressed MVT body** against a committed golden. Any coordinate that
//! moves by one unit in one tile fails it.
//!
//! # The fixture
//!
//! `tests/fixtures/guard/br-clip-divergence.parquet` — 3,443 real Brazil field
//! polygons, all of the features in three 0.2°-square windows centred on the
//! three locations where the i_overlay 8→9 A/B actually diverged
//! ((-43.3004, -8.5125), (-44.2621, -14.2955), (-50.8011, -14.534)). It is a
//! subset of a 82,714-feature extract that was *verified* to produce different
//! tile bytes under i_overlay 8 vs 9 on a full z0–z13 run.
//!
//! The windows are kept **whole** — every feature whose bbox intersects one is
//! present, none are sampled — because dropping neighbours of a divergent
//! polygon is exactly how you would lose it. See
//! `tests/fixtures/guard/README.md` for the extraction query.
//!
//! **Discrimination is verified, and the margin is thin.** Rolling our direct
//! dependency back (`i_overlay = "=8.1.2"` in `crates/core/Cargo.toml`, then
//! `cargo update -p i_overlay@9.0.0 --precise 8.1.2`, which also moves
//! `i_float` 5 → 4.1.0 and `i_shape` 5 → 4.0.0 and compiles unchanged) makes
//! this test fail with 4 of 384 guarded tiles differing: `8/96/138` and
//! `8/97/134`, in *both* cases. So only two z8 tiles discriminate that bump,
//! and the `ioverlay` case caught nothing `defaults` did not (the Brazil A/B
//! moved 18 tiles across z5–z8). Widening the fixture around more divergent
//! sites would harden it; the source extract is not in the repo, so that is
//! optional future work.
//!
//! The build requests z0–z13, but this fixture emits **no tiles at z0–z2**
//! (the golden starts at `3/3/4`), so a change confined to z0–z2 is invisible
//! to this guard.
//!
//! # The two cases
//!
//! `simple_clip_fastpath` defaults to `true`, which routes ~94% of fine-zoom
//! polygon clips through Sutherland–Hodgman and never touches i_overlay
//! (#239). A golden taken only at the defaults would therefore leave the
//! boolean-ops engine mostly dark — the same blind spot in a new place. So
//! there are two cases:
//!
//! * `defaults` — what users actually get, and what the #558 A/B measured.
//! * `ioverlay` — `simple_clip_fastpath: false`, so **every** polygon clip goes
//!   through `i_overlay`. This is the discriminating one for engine bumps.
//!
//! # Regenerating the golden
//!
//! When an output change is intended (a deliberate algorithm change, or an
//! accepted geometry-engine bump), regenerate in one command from the
//! workspace root:
//!
//! ```text
//! TYLERTOO_UPDATE_GOLDEN=1 cargo test -p tylertoo-core --test convert_guard_golden
//! ```
//!
//! Only the exact value `1` regenerates. It rewrites
//! `tests/fixtures/guard/br-clip-divergence.golden.txt` in place and fails the
//! run, so the regeneration can never be mistaken for a pass. Commit
//! the diff **with the change that caused it** and say in the PR body why the
//! output moved — a golden diff arriving on its own, or with a dependency bump
//! and no explanation, is the thing this guard exists to stop.
//!
//! # Determinism
//!
//! The golden is a set of tile-body digests, and tylertoo promises the exported
//! archive is byte-identical across thread counts (#423/#508, asserted by
//! `crates/cli/tests/thread_count_determinism.rs`) and that `partition_wave` is
//! a scheduling knob only. The digests are of the **decompressed** MVT bodies,
//! so a gzip encoder change would not be mistaken for a geometry change.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tylertoo_core::archive_index::ArchiveIndex;
use tylertoo_core::compression;
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

/// The committed fixture, and the golden beside it.
const FIXTURE: &str = "br-clip-divergence.parquet";
const GOLDEN: &str = "br-clip-divergence.golden.txt";

/// One layer name for every case, so the golden lines differ only where the
/// geometry does.
const LAYER: &str = "guard";

/// Zoom depth of the guarded build: the same z0–z13 the #558 A/B ran at.
/// (z0–z2 produce no tiles for this fixture; the golden starts at z3.)
///
/// It is not just about covering the zooms that diverged there (z5–z8). A
/// clipper is only exercised where a tile edge cuts a feature, and the three
/// windows are 0.2° across — at z5 a whole window sits inside one tile and
/// almost nothing is clipped, while at z13 (tile width 0.044°) each window is a
/// ~5x5 block of tiles whose interior seams cut the polygon fabric everywhere.
/// The deep zooms are where the clip coverage is. 384 tiles, ~0.5 s release,
/// ~20 s debug.
const MIN_ZOOM: u8 = 0;
const MAX_ZOOM: u8 = 13;

/// The env switch that rewrites the golden. See the module header.
const UPDATE_ENV: &str = "TYLERTOO_UPDATE_GOLDEN";

/// One guarded build: a name for the golden's first column and the one knob
/// that differs between them.
struct Case {
    name: &'static str,
    /// `false` forces every polygon clip through `i_overlay` (#239).
    simple_clip_fastpath: bool,
}

const CASES: &[Case] = &[
    Case {
        name: "defaults",
        simple_clip_fastpath: true,
    },
    Case {
        name: "ioverlay",
        simple_clip_fastpath: false,
    },
];

/// The conversion knobs, fixed. Everything not named here is a default, and
/// a default that moves is an output change this guard should catch.
fn convert_options() -> ConvertOptions {
    ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: MIN_ZOOM,
            max_zoom: MAX_ZOOM,
        },
        ..Default::default()
    }
}

fn export_options(case: &Case) -> ExportOptions {
    ExportOptions {
        layer_name: LAYER.to_string(),
        simple_clip_fastpath: case.simple_clip_fastpath,
        ..Default::default()
    }
}

/// Run one case end to end and return its golden lines.
///
/// Each line is `<case> <z>/<x>/<y> <decompressed bytes> <xxh3-64 hex>`, sorted
/// by (z, x, y) — a stable, reviewable text diff rather than an opaque binary
/// blob, so a golden update shows *which* tiles moved.
///
/// `pinned_scheduling` forces the two knobs whose defaults are derived from the
/// *machine* rather than from the input — the pass-2 reader count
/// ([`ConvertOptions::read_workers`], `READ_WORKERS_AUTO`) and the export
/// partition wave ([`ExportOptions::partition_wave`], `PARTITION_WAVE_AUTO`,
/// which is resolved against available RAM). Both are documented as
/// output-neutral; `golden_does_not_depend_on_machine_shaped_scheduling`
/// spends one extra build proving it, because if either were not, this golden
/// would be a machine fingerprint rather than a tiling fingerprint.
fn golden_lines(fixture: &Path, case: &Case, pinned_scheduling: bool) -> Vec<String> {
    let dir = tempfile::tempdir().expect("tempdir");
    let overview = dir.path().join("ov.parquet");
    let archive = dir.path().join("out.pmtiles");

    let mut convert = convert_options();
    let mut export = export_options(case);
    if pinned_scheduling {
        convert.read_workers = 1;
        export.partition_wave = 1;
    }

    convert_to_overviews(fixture, &overview, &convert)
        .unwrap_or_else(|e| panic!("[{}] convert: {e}", case.name));
    export_pmtiles(&overview, &archive, &export)
        .unwrap_or_else(|e| panic!("[{}] export: {e}", case.name));

    let idx = ArchiveIndex::open(&archive).expect("open archive");
    let tile_compression = idx.header().tile_compression;

    // BTreeMap, not the iterator's order: the golden must be sorted by tile
    // coordinate whatever order the archive happens to list entries in.
    let mut bodies: BTreeMap<(u8, u32, u32), Vec<u8>> = BTreeMap::new();
    for t in idx.tiles() {
        let t = t.expect("tile entry");
        let raw = idx.read_range(t.range.clone()).expect("tile body");
        // Digest the MVT, not the gzip stream: a codec change is not a
        // geometry change and must not read as one.
        let plain =
            compression::decompress_capped(&raw, tile_compression, compression::MAX_TILE_BYTES)
                .expect("decompress tile");
        assert!(
            bodies.insert((t.z, t.x, t.y), plain).is_none(),
            "[{}] archive lists tile {}/{}/{} twice",
            case.name,
            t.z,
            t.x,
            t.y
        );
    }
    assert!(
        !bodies.is_empty(),
        "[{}] the guarded build produced no tiles — the fixture or the \
         pipeline is broken, and an empty golden would guard nothing",
        case.name
    );

    bodies
        .into_iter()
        .map(|((z, x, y), body)| {
            format!(
                "{} {z}/{x}/{y} {} {:016x}",
                case.name,
                body.len(),
                xxhash_rust::xxh3::xxh3_64(&body),
            )
        })
        .collect()
}

fn golden_path() -> PathBuf {
    // Beside the fixture, whose locator already resolves the workspace root.
    fixture::guard(FIXTURE).with_file_name(GOLDEN)
}

fn render(lines: &[String]) -> String {
    let mut out = String::new();
    out.push_str(
        "# tylertoo convert-guard golden (#558) — per-tile xxh3-64 of the \
         decompressed MVT body.\n\
         # Regenerate with: TYLERTOO_UPDATE_GOLDEN=1 cargo test -p tylertoo-core \
         --test convert_guard_golden\n\
         # Format: <case> <z>/<x>/<y> <bytes> <xxh3-64>\n",
    );
    for line in lines {
        let _ = writeln!(out, "{line}");
    }
    out
}

/// The guard.
#[test]
fn guarded_build_matches_the_committed_tile_golden() {
    let fixture_path = fixture::guard(FIXTURE);
    let mut lines = Vec::new();
    for case in CASES {
        lines.extend(golden_lines(&fixture_path, case, false));
    }
    let current = render(&lines);
    let path = golden_path();

    // Only the exact value `1` regenerates: `=0` or `=false` must not
    // silently rewrite the golden.
    if std::env::var(UPDATE_ENV).as_deref() == Ok("1") {
        std::fs::write(&path, &current).expect("write golden");
        // Fail loudly: regenerating is not passing. The next unswitched run
        // is what proves the new golden is stable.
        panic!(
            "{UPDATE_ENV} set — rewrote {}. Re-run without it to verify, and \
             commit the diff with the change that caused it.",
            path.display()
        );
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "no golden at {} ({e}). Create it with:\n  \
             {UPDATE_ENV}=1 cargo test -p tylertoo-core --test convert_guard_golden",
            path.display()
        )
    });

    if current == expected {
        return;
    }

    // Report the drift as tiles, not as a wall of text: which tiles moved is
    // the whole diagnosis.
    let expected_tiles: BTreeMap<&str, &str> = tile_map(&expected);
    let current_tiles: BTreeMap<&str, &str> = tile_map(&current);
    let mut moved: Vec<String> = Vec::new();
    for (key, exp) in &expected_tiles {
        match current_tiles.get(key) {
            Some(cur) if cur == exp => {}
            Some(cur) => moved.push(format!("  {key}: {exp}  ->  {cur}")),
            None => moved.push(format!("  {key}: {exp}  ->  (tile gone)")),
        }
    }
    for key in current_tiles.keys() {
        if !expected_tiles.contains_key(key) {
            moved.push(format!("  {key}: (absent)  ->  {}", current_tiles[key]));
        }
    }
    // Every tile matches but the files differ: only the preamble moved (the
    // regeneration command in the header was reworded, say). Say that, rather
    // than reporting "0 tiles differ" and leaving the reader hunting.
    assert!(
        !moved.is_empty(),
        "the golden's {} tile digests all match, but its preamble text does \
         not. Nothing about the output changed; refresh the file with:\n  \
         {UPDATE_ENV}=1 cargo test -p tylertoo-core --test convert_guard_golden",
        expected_tiles.len()
    );

    let shown = moved.len().min(40);
    panic!(
        "TILE OUTPUT CHANGED — {} of {} guarded tiles differ from the golden.\n\
         \n\
         If this arrived with a dependency bump (geo, geo-types, i_overlay, \
         i_float, i_shape, earcut), that bump changed rendered geometry: it is \
         not an auto-merge, it needs a maintainer decision (#558).\n\
         If the change is intended, regenerate and commit the diff with it:\n  \
         {UPDATE_ENV}=1 cargo test -p tylertoo-core --test convert_guard_golden\n\
         \n{}{}",
        moved.len(),
        expected_tiles.len(),
        moved[..shown].join("\n"),
        if moved.len() > shown {
            format!("\n  ... and {} more", moved.len() - shown)
        } else {
            String::new()
        },
    );
}

/// A golden is only a regression guard if it is a function of the input and
/// the knobs. Two of the defaults it is taken at are functions of the
/// **machine**: `read_workers` (auto = available parallelism) and
/// `partition_wave` (auto = resolved against available RAM). If either reached
/// the output, this file would be a fingerprint of the runner that wrote it,
/// and every CI lane with a different core count or RAM would "fail" the
/// guard for no reason at all — the fastest possible way to get a guard
/// disabled.
///
/// So pin both to 1 and re-derive the `defaults` case: same digests, or the
/// determinism promise in `context/ARCHITECTURE.md` is not true and this
/// golden cannot be trusted. Thread-count determinism proper is #423's job
/// (`crates/cli/tests/thread_count_determinism.rs`); this is the narrower
/// claim this file rests on.
#[test]
fn golden_does_not_depend_on_machine_shaped_scheduling() {
    let fixture_path = fixture::guard(FIXTURE);
    let case = &CASES[0];
    assert_eq!(case.name, "defaults");

    let auto = golden_lines(&fixture_path, case, false);
    let pinned = golden_lines(&fixture_path, case, true);

    let differing: Vec<String> = auto
        .iter()
        .zip(&pinned)
        .filter(|(a, p)| a != p)
        .map(|(a, p)| format!("  auto: {a}\n  pin1: {p}"))
        .collect();
    assert_eq!(
        auto.len(),
        pinned.len(),
        "auto scheduling produced {} tiles, pinned produced {} — \
         read_workers/partition_wave changed the tile SET, so the golden is a \
         machine fingerprint",
        auto.len(),
        pinned.len()
    );
    assert!(
        differing.is_empty(),
        "{} tile(s) differ between auto and pinned scheduling — \
         read_workers/partition_wave reached the output, so the golden cannot \
         be portable:\n{}",
        differing.len(),
        differing.join("\n")
    );
}

/// `"<case> <z>/<x>/<y>" -> "<bytes> <digest>"` for every non-comment line.
fn tile_map(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| {
            // case + coord is the key (2 fields), bytes + digest the value.
            let mut it = l.char_indices().filter(|(_, c)| *c == ' ').map(|(i, _)| i);
            it.next()?;
            let split = it.next()?;
            Some((&l[..split], l[split + 1..].trim()))
        })
        .collect()
}
