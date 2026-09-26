//! The parity oracle for native sharded builds (#498).
//!
//! A sharded build is only worth having if it is *indistinguishable* from the
//! monolithic one. Tile counts matching is not enough — a seam bug moves
//! geometry between neighbouring tiles while keeping every count identical —
//! so the bar here is the strongest one available:
//!
//! 1. the same tile-id set, per zoom;
//! 2. the same per-zoom tile counts;
//! 3. **byte-identical tile bodies**, tile by tile.
//!
//! against a single monolithic `convert` → `export` of the same input with the
//! same options. Anything that diverges at a seam — a feature clipped against
//! the wrong window, a winner byte read at the wrong row after the shard
//! re-addressed the plan, a tile emitted by both shards or by neither — fails
//! loudly here.
//!
//! Two fixtures, because one cannot cover both halves of the risk:
//!
//! * **Madagascar admin-4** (17,465 real polygons, z0–z9, pivot z6, N = 2 and
//!   3) — messy geometry with seams running through it. Its single row group
//!   and absent `geo` metadata mean nothing prunes, so this is purely about
//!   clipping and emission at a seam.
//! * **`sharding-grid.parquet`** (1,440 synthetic polygons over the whole
//!   world in 20 row groups *with* covering statistics, z0–z5, pivot z3,
//!   N = 2, 3 and 4) — here every shard prunes, so the shared convert plan's
//!   row-indexed winner table has to be re-addressed onto a narrower row
//!   stream. That is the step where an off-by-one row group produces an
//!   archive that is complete, plausible and quietly wrong at every level.
//!
//! Both are deliberately not `#[ignore]`d: this is the acceptance test for the
//! whole feature, and a sharded build that is not proven identical is not a
//! sharded build.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tylertoo_core::archive_index::ArchiveIndex;
use tylertoo_core::merge::{merge_shards, MergeOptions};
use tylertoo_core::overview::cluster::{AccumulateOp, AccumulateSpec};
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
use tylertoo_core::overview::simplify::{CollapseMode, SimplifyOptions};
use tylertoo_core::shard::{ShardPlan, TileRange};

#[path = "../../../tests/support/fixture.rs"]
mod fixture;

/// One layer name across every job, so `merge_shards` unions one
/// `vector_layers` entry rather than declaring several.
const LAYER: &str = "parity";

/// One build under test: which input, how deep a pyramid, where the shards are
/// cut and how many of them there are.
#[derive(Debug, Clone, Copy)]
struct Build {
    min_zoom: u8,
    max_zoom: u8,
    pivot: u8,
    shards: usize,
    /// Whether this input's row groups carry usable covering statistics, so
    /// the shards can prune their reads and the shared convert plan has to be
    /// re-addressed onto a narrower row stream. Asserted, not assumed: a
    /// fixture that silently stopped pruning would turn the most delicate
    /// path in a sharded build into dead code.
    expect_row_group_pruning: bool,
    /// Which of the plan's row-indexed side tables this build exercises.
    flavor: Flavor,
    /// #541: run the coarse job with a **convert-side level ceiling**, so it
    /// materializes only the levels it exports instead of the whole
    /// monolithic pyramid.
    ///
    /// Two things then have to hold, and both are asserted: the capped job's
    /// tiles are still byte-identical to the monolithic ones (the ladder
    /// cascade folds canonical geometry through every finer level's GSD, so
    /// skipping those levels' OUTPUT must not skip their fold STEPS), and the
    /// convert plan it saves is **byte-identical** to the one a full coarse
    /// job saves — the data shards consume that plan unchanged, and a ceiling
    /// that leaked into it would give the whole fleet a different pyramid.
    coarse_level_ceiling: bool,
}

/// The same build with #541's convert-side ceiling on the coarse job.
fn capped(build: Build) -> Build {
    Build {
        coarse_level_ceiling: true,
        ..build
    }
}

/// The conversion knob set under test.
///
/// `rebase_plan_for_shard` moves FOUR row-indexed sections onto the shard's
/// narrower stream — the winner table, the geometry kinds, the per-level
/// carriers and the cluster tables — and each one is its own opportunity for
/// an off-by-one. A single flavor would leave three of them dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flavor {
    /// Coalescing off, everything else default: the winner table and the
    /// geometry kinds.
    NoCoalesce,
    /// **Every knob at its default**, coalescing included. The important
    /// case, and the one a reviewer PoC'd as a panic: `coalesce_lines` is on
    /// by default, so a polygon dataset saves a coalesce section that is
    /// present and empty, and the re-addressing has to accept that rather
    /// than assert the section absent.
    Defaults,
    /// `--collapse-square`, which turns on the tiny-polygon accumulator: the
    /// plan then carries per-level CARRIER row lists, which are re-keyed by
    /// the same run map.
    Carriers,
    /// `--cluster --accumulate-attribute`, on a point input: the plan carries
    /// per-level CLUSTER TABLES keyed by source row, which the re-addressing
    /// has to re-key (and drop the keys this shard does not read).
    ClusteredPoints,
}

/// Pass-2 reader threads for every convert this oracle runs.
///
/// `TYLERTOO_TEST_READ_WORKERS` overrides the default so the whole suite can
/// be run against the sequential reader (`1`) and against #494's parallel
/// segment merge (`4`). That distinction matters here more than anywhere
/// else: the parallel merge re-derives each batch's `row_offset` by counting
/// rows over the merged stream from zero, and a shard's stream is the pruned
/// one — so a merge that ever counted ABSOLUTE file rows instead would give
/// every segment after the first the wrong winner byte, on pruning shards
/// only. Byte parity under both settings is what rules that out.
fn read_workers() -> usize {
    std::env::var("TYLERTOO_TEST_READ_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(tylertoo_core::overview::convert::READ_WORKERS_AUTO)
}

fn convert_options(build: Build) -> ConvertOptions {
    let levels = LevelPlan::ZoomRange {
        min_zoom: build.min_zoom,
        max_zoom: build.max_zoom,
    };
    let read_workers = read_workers();
    match build.flavor {
        // A chain is a new geometry spanning every row it merged, which no
        // single row group's bbox bounds — so `--shard` refuses a plan that
        // carries one. Turning it off keeps this flavor honest about what it
        // is testing; `Flavor::Defaults` covers the on-by-default case, where
        // a polygon input produces an empty-but-present section.
        Flavor::NoCoalesce => ConvertOptions {
            levels,
            read_workers,
            coalesce_lines: false,
            ..Default::default()
        },
        Flavor::Defaults => ConvertOptions {
            levels,
            read_workers,
            ..Default::default()
        },
        Flavor::Carriers => ConvertOptions {
            levels,
            read_workers,
            coalesce_lines: false,
            simplify: SimplifyOptions {
                collapse: CollapseMode::Square,
                ..Default::default()
            },
            ..Default::default()
        },
        Flavor::ClusteredPoints => ConvertOptions {
            levels,
            read_workers,
            coalesce_lines: false,
            cluster: true,
            accumulate: vec![AccumulateSpec {
                column: "weight".to_string(),
                op: AccumulateOp::Sum,
            }],
            ..Default::default()
        },
    }
}

fn export_options() -> ExportOptions {
    ExportOptions {
        layer_name: LAYER.to_string(),
        ..Default::default()
    }
}

/// Every tile in an archive: `zoom -> tile id -> body bytes`.
///
/// Read through [`ArchiveIndex`], the same reader `merge_shards` uses, so the
/// comparison sees the archives exactly as a client would.
fn tiles_of(path: &Path) -> BTreeMap<u8, BTreeMap<u64, Vec<u8>>> {
    let idx = ArchiveIndex::open(path).expect("open archive");
    let mut refs = Vec::new();
    for t in idx.tiles() {
        refs.push(t.expect("tile entry"));
    }
    let mut out: BTreeMap<u8, BTreeMap<u64, Vec<u8>>> = BTreeMap::new();
    for t in refs {
        let body = idx.read_range(t.range.clone()).expect("tile body");
        assert!(
            out.entry(t.z).or_default().insert(t.id, body).is_none(),
            "archive {} lists tile id {} twice",
            path.display(),
            t.id
        );
    }
    out
}

/// Build one shard: convert the subset of row groups its range reaches
/// (re-addressing the shared plan onto them), then export only its tiles.
fn build_shard(
    input: &Path,
    dir: &Path,
    plan: &Path,
    range: TileRange,
    index: usize,
    build: Build,
) -> PathBuf {
    let overview = dir.join(format!("shard-{index}.parquet"));
    let archive = dir.join(format!("shard-{index}.pmtiles"));
    convert_to_overviews(
        input,
        &overview,
        &ConvertOptions {
            plan: Some(plan.to_path_buf()),
            shard: Some(range),
            ..convert_options(build)
        },
    )
    .unwrap_or_else(|e| panic!("shard {index} convert: {e}"));
    export_pmtiles(
        &overview,
        &archive,
        &ExportOptions {
            tile_range: Some(range),
            // The shard owns no zoom coarser than the pivot, and says so
            // rather than claiming the coarse job's half.
            min_zoom: Some(build.pivot),
            ..export_options()
        },
    )
    .unwrap_or_else(|e| panic!("shard {index} export: {e}"));
    archive
}

/// #541: run the coarse job with a convert-side level ceiling, and assert
/// the two things the fleet depends on before handing its overview back.
///
/// `full_plan` is the plan the same job saved WITHOUT the ceiling. The capped
/// job must save that file byte for byte: a data shard verifies the
/// fingerprint and then reads the plan's row-indexed tables verbatim, so "the
/// coarse job may build fewer levels" is only true if the artifact does not
/// move. The second assertion is that the cap actually bit — otherwise the
/// tile-body parity the caller goes on to check would prove nothing about
/// #541.
fn level_capped_coarse_overview(
    input: &Path,
    dir: &Path,
    build: Build,
    full_plan: &Path,
) -> PathBuf {
    let pivot = build.pivot;
    let capped_overview = dir.join("coarse.parquet");
    let capped_plan = dir.join("coarse.plan");
    let report = convert_to_overviews(
        input,
        &capped_overview,
        &ConvertOptions {
            save_plan: Some(capped_plan.clone()),
            zoom_ceiling: Some(pivot - 1),
            ..convert_options(build)
        },
    )
    .expect("level-capped coarse convert");
    assert_eq!(
        std::fs::read(full_plan).expect("full plan"),
        std::fs::read(&capped_plan).expect("capped plan"),
        "the level-capped coarse job must save the plan a full coarse job saves"
    );
    assert!(
        report
            .levels
            .iter()
            .all(|l| l.zoom.is_some_and(|z| z < pivot)),
        "the capped coarse job built a level at or past the pivot z{pivot}: {:?}",
        report.levels.iter().map(|l| l.zoom).collect::<Vec<_>>()
    );
    eprintln!(
        "[oracle] level ceiling z{}: coarse job built {} level(s), plan byte-identical",
        pivot - 1,
        report.levels.len()
    );
    capped_overview
}

/// The oracle.
fn assert_sharded_build_matches_monolithic(input: &Path, build: Build) {
    let Build {
        min_zoom,
        max_zoom: _,
        pivot,
        shards,
        expect_row_group_pruning,
        flavor,
        coarse_level_ceiling,
    } = build;
    let dir = tempfile::tempdir().expect("tempdir");
    let dir = dir.path();

    // --- Step 0: cut the shard plan (footer only). ---------------------
    let source = tylertoo_core::input_set::ConvertSource::resolve_path(input).expect("source");
    let shard_plan = ShardPlan::compute(&source, pivot, shards).expect("cut the shard plan");
    assert_eq!(shard_plan.shards(), shards);
    let shard_plan_path = dir.join("shards.json");
    shard_plan.save(&shard_plan_path).expect("save shard plan");

    // The premise the re-addressing exists for: on an input whose row groups
    // carry covering statistics, at least one shard reads strictly fewer row
    // groups than the plan was saved over — so the plan's winner table is
    // addressed by a row stream the shard does not have, and has to be moved.
    let total_groups = source.num_row_groups_total().expect("row group count");
    let pruned = (0..shards).any(|i| {
        let b = shard_plan.range(i).expect("range").read_bounds();
        let sel = source
            .select_row_groups(&[b.lng_min, b.lat_min, b.lng_max, b.lat_max])
            .expect("prune");
        sel.total_selected() < total_groups
    });
    assert_eq!(
        pruned,
        expect_row_group_pruning,
        "row-group pruning on {}: expected {expect_row_group_pruning}, got {pruned} \
         ({total_groups} row group(s) total). A fixture that stopped pruning would leave the \
         plan re-addressing untested.",
        input.display()
    );

    // --- Step 1: the coarse job. ---------------------------------------
    // It reads the whole input, writes the convert plan every shard consumes,
    // and exports the zooms below the pivot. The UNCAPPED coarse job's
    // overview file is also, by construction, exactly the monolithic one —
    // same input, same options — so the control export below reuses it rather
    // than converting twice.
    let convert_plan = dir.join("convert.plan");
    let overview = dir.join("mono.parquet");
    convert_to_overviews(
        input,
        &overview,
        &ConvertOptions {
            save_plan: Some(convert_plan.clone()),
            ..convert_options(build)
        },
    )
    .expect("coarse convert");

    // #541: the same job again, this time materializing only the levels it
    // exports. Everything the fleet depends on has to be unchanged.
    let coarse_overview = if coarse_level_ceiling {
        level_capped_coarse_overview(input, dir, build, &convert_plan)
    } else {
        overview.clone()
    };

    let coarse = dir.join("coarse.pmtiles");
    export_pmtiles(
        &coarse_overview,
        &coarse,
        &ExportOptions {
            zoom_ceiling: Some(pivot - 1),
            min_zoom: Some(min_zoom),
            ..export_options()
        },
    )
    .expect("coarse export");

    // --- Step 2: the shards, one archive each. -------------------------
    let mut archives = vec![coarse];
    for i in 0..shards {
        let range = shard_plan.range(i).expect("shard range");
        archives.push(build_shard(input, dir, &convert_plan, range, i, build));
    }

    // --- Step 3: one merge of coarse + shards. -------------------------
    // Zoom-disjoint IS id-disjoint (ids ascend with zoom), so the coarse
    // archive needs no special handling: `merge_shards` validates per tile id
    // and the coarse ids simply all sort first.
    let merged = dir.join("merged.pmtiles");
    let report = merge_shards(
        &archives,
        &merged,
        &MergeOptions {
            work_dir: Some(dir.to_path_buf()),
        },
    )
    .expect("merge coarse + shards");

    // --- The control: one monolithic export of the same overview. ------
    let mono = dir.join("mono.pmtiles");
    export_pmtiles(
        &overview,
        &mono,
        &ExportOptions {
            min_zoom: Some(min_zoom),
            ..export_options()
        },
    )
    .expect("monolithic export");

    // --- Assertions ----------------------------------------------------
    let want = tiles_of(&mono);
    let got = tiles_of(&merged);
    assert!(
        !want.is_empty(),
        "the control archive is empty; the oracle would prove nothing"
    );
    eprintln!(
        "[oracle] {} pivot z{pivot} N={shards}: {} tile(s) over z{:?}",
        input.file_name().unwrap().to_string_lossy(),
        want.values().map(BTreeMap::len).sum::<usize>(),
        want.keys().collect::<Vec<_>>(),
    );

    // (0) THE SEAM PROPERTY, stated directly rather than inferred from the
    // merge succeeding: every pair of jobs holds disjoint tile ids, and
    // together they hold exactly the monolithic set. `merge_shards` already
    // refuses a duplicate id, so a dupe would have failed above — but a GAP
    // would not, and a gap is what an off-by-one in `ids_at` produces. This
    // is also where "a feature straddling two shards lands in exactly the
    // tiles each range owns" is pinned: both neighbours read that feature,
    // and if either emitted one tile too many or too few, the union or the
    // disjointness below breaks.
    let per_job: Vec<BTreeMap<u8, BTreeMap<u64, Vec<u8>>>> =
        archives.iter().map(|p| tiles_of(p)).collect();
    for (a, job_a) in per_job.iter().enumerate() {
        for (b, job_b) in per_job.iter().enumerate().skip(a + 1) {
            for (z, tiles) in job_a {
                if let Some(other) = job_b.get(z) {
                    let shared: Vec<u64> = tiles
                        .keys()
                        .filter(|id| other.contains_key(id))
                        .copied()
                        .collect();
                    assert!(
                        shared.is_empty(),
                        "N={shards}: jobs {a} and {b} both hold z{z} tile(s) {:?} — shards must \
                         be disjoint by construction",
                        shared.iter().take(5).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
    let union: usize = per_job
        .iter()
        .map(|j| j.values().map(BTreeMap::len).sum::<usize>())
        .sum();
    assert_eq!(
        union,
        want.values().map(BTreeMap::len).sum::<usize>(),
        "N={shards}: the jobs' tiles must cover the monolithic set exactly — no gap at a seam, \
         no tile built twice"
    );

    // (1) the same zooms, and the pivot really does fall inside them — a
    // fixture or zoom range that put every tile on one side of the pivot
    // would make this test vacuous.
    assert_eq!(
        want.keys().copied().collect::<Vec<_>>(),
        got.keys().copied().collect::<Vec<_>>(),
        "N={shards}: the merged archive must hold exactly the monolithic zooms"
    );
    assert!(
        want.keys().any(|&z| z < pivot) && want.keys().any(|&z| z >= pivot),
        "the fixture must straddle the pivot for this oracle to mean anything; zooms: {:?}",
        want.keys().collect::<Vec<_>>()
    );

    // (2) per-zoom tile counts, including the merge report's own tally.
    for (&z, tiles) in &want {
        assert_eq!(
            got[&z].len(),
            tiles.len(),
            "N={shards}: z{z} tile count differs (merged {} vs monolithic {})",
            got[&z].len(),
            tiles.len()
        );
        assert_eq!(
            report.per_zoom_tile_counts.get(&z).copied().unwrap_or(0),
            tiles.len() as u64,
            "N={shards}: z{z} merge report disagrees with the archive it wrote"
        );
    }

    // (3) the same tile ids, and byte-identical bodies. A seam bug that moved
    // geometry between two neighbouring tiles would survive (1) and (2) and
    // die here.
    for (&z, tiles) in &want {
        let mine = &got[&z];
        let missing: Vec<u64> = tiles
            .keys()
            .filter(|id| !mine.contains_key(id))
            .collect::<Vec<_>>()
            .into_iter()
            .copied()
            .collect();
        let extra: Vec<u64> = mine
            .keys()
            .filter(|id| !tiles.contains_key(id))
            .copied()
            .collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "N={shards}: z{z} tile-id set differs — {} missing (first {:?}), {} extra (first {:?})",
            missing.len(),
            missing.iter().take(5).collect::<Vec<_>>(),
            extra.len(),
            extra.iter().take(5).collect::<Vec<_>>()
        );
        for (id, body) in tiles {
            assert_eq!(
                &mine[id],
                body,
                "N={shards}: z{z} tile id {id} differs in {} vs {} bytes — a shard seam moved \
                 geometry",
                mine[id].len(),
                body.len()
            );
        }
    }

    assert_archive_metadata_matches(&mono, &merged, shards);
    eprintln!("[oracle] {flavor:?} N={shards}: header, bounds and vector_layers all match");
}

/// Assertion (4): THE ARCHIVE, not just the tiles.
///
/// A client reads the header and the metadata before it reads a tile, so a
/// merged archive whose bodies match but whose header declares the wrong
/// zooms — or whose `vector_layers` came out of an empty shard's z0..z0
/// sentinel — is still wrong for every consumer. Checked against the
/// monolithic archive, which is the definition of right.
///
/// NOTE what is deliberately NOT asserted: the two FILES are not equal byte
/// for byte. The merged one is assembled from N archives, so its directory
/// layout, its dedup accounting and its tile ORDER within a zoom differ, and
/// (today) it carries no tilestats. Tile bodies are identical; the container
/// is not. See `docs/diving-deeper/sharded-builds.md`.
fn assert_archive_metadata_matches(mono: &Path, merged: &Path, shards: usize) {
    let (want_h, got_h) = (
        ArchiveIndex::open(mono)
            .expect("open mono")
            .header()
            .clone(),
        ArchiveIndex::open(merged)
            .expect("open merged")
            .header()
            .clone(),
    );
    assert_eq!(
        (got_h.min_zoom, got_h.max_zoom),
        (want_h.min_zoom, want_h.max_zoom),
        "N={shards}: the merged header must declare the monolithic zoom range"
    );
    let want_b = ArchiveIndex::open(mono).expect("open mono").bounds();
    let got_b = ArchiveIndex::open(merged).expect("open merged").bounds();
    match (want_b, got_b) {
        (Some(w), Some(g)) => {
            // The header stores bounds as E7 integers, so the union of the
            // shards' clipped extents round-trips to the same E7 values as
            // the monolithic extent rather than merely close ones.
            for (name, w, g) in [
                ("lng_min", w.lng_min, g.lng_min),
                ("lat_min", w.lat_min, g.lat_min),
                ("lng_max", w.lng_max, g.lng_max),
                ("lat_max", w.lat_max, g.lat_max),
            ] {
                assert!(
                    (w - g).abs() < 1e-6,
                    "N={shards}: merged bounds {name} {g} != monolithic {w} — the shards' \
                     clipped extents must union back to the whole"
                );
            }
        }
        (w, g) => panic!("N={shards}: bounds presence differs: monolithic {w:?}, merged {g:?}"),
    }
    let layers = |p: &Path| -> Vec<(String, u64, u64)> {
        let meta = ArchiveIndex::open(p)
            .expect("open")
            .metadata_json()
            .expect("metadata is JSON");
        let mut out: Vec<(String, u64, u64)> = meta["vector_layers"]
            .as_array()
            .expect("vector_layers")
            .iter()
            .map(|l| {
                (
                    l["id"].as_str().expect("layer id").to_string(),
                    l["minzoom"].as_u64().expect("minzoom"),
                    l["maxzoom"].as_u64().expect("maxzoom"),
                )
            })
            .collect();
        out.sort();
        out
    };
    assert_eq!(
        layers(merged),
        layers(mono),
        "N={shards}: the merged archive must declare the monolithic layers and zoom spans"
    );
}

/// A world-spanning POINT grid with covering statistics, written at test
/// time.
///
/// The committed fixtures are all polygons or lines, and clustering (Q4) only
/// engages on points — so without this the cluster-table half of the shard
/// re-addressing has no coverage at all. Built here rather than committed
/// because it is 1,200 points of pure arithmetic: cheaper to generate than to
/// carry, and impossible to leave stale.
///
/// Shaped like the polygon grid it mirrors: sorted by longitude so each row
/// group is a narrow, prunable vertical band, with a `bbox` struct declared as
/// the GeoParquet 1.1 covering so the footers carry usable envelopes.
fn write_point_grid(path: &Path) {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, BinaryArray, Float64Array, Int64Array, RecordBatch, StructArray};
    use arrow_schema::{DataType, Field, Fields, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;

    const COLS: i64 = 60;
    const ROWS: i64 = 20;
    let (lon_min, lon_max) = (-175.0f64, 175.0f64);
    let (lat_min, lat_max) = (-70.0f64, 70.0f64);
    let dx = (lon_max - lon_min) / COLS as f64;
    let dy = (lat_max - lat_min) / ROWS as f64;

    let mut rows: Vec<(i64, f64, f64, f64)> = Vec::new();
    for j in 0..ROWS {
        for i in 0..COLS {
            // Three points per cell, close enough together that the coarse
            // levels really do cluster them.
            for k in 0..3i64 {
                let x = lon_min + dx * (i as f64 + 0.25 + 0.15 * k as f64);
                let y = lat_min + dy * (j as f64 + 0.25 + 0.15 * k as f64);
                let id = (j * COLS + i) * 3 + k;
                rows.push((id, ((i * 7 + j * 13 + k) % 97) as f64, x, y));
            }
        }
    }
    // Longitude-sorted: narrow, prunable row-group bands.
    rows.sort_by(|a, b| a.2.partial_cmp(&b.2).unwrap().then(a.0.cmp(&b.0)));

    let wkb = |x: f64, y: f64| -> Vec<u8> {
        let mut out = Vec::with_capacity(21);
        out.push(1u8); // little endian
        out.extend_from_slice(&1u32.to_le_bytes()); // Point
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
        out
    };

    let bbox_fields: Fields = vec![
        Field::new("xmin", DataType::Float64, false),
        Field::new("ymin", DataType::Float64, false),
        Field::new("xmax", DataType::Float64, false),
        Field::new("ymax", DataType::Float64, false),
    ]
    .into();
    let schema = Arc::new(Schema::new(vec![
        Field::new("cell_id", DataType::Int64, false),
        Field::new("weight", DataType::Float64, false),
        Field::new("geometry", DataType::Binary, false),
        Field::new("bbox", DataType::Struct(bbox_fields.clone()), false),
    ]));

    let ids: ArrayRef = Arc::new(Int64Array::from(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
    ));
    let weights: ArrayRef = Arc::new(Float64Array::from(
        rows.iter().map(|r| r.1).collect::<Vec<_>>(),
    ));
    let geoms: Vec<Vec<u8>> = rows.iter().map(|r| wkb(r.2, r.3)).collect();
    let geometry: ArrayRef = Arc::new(BinaryArray::from(
        geoms.iter().map(|g| g.as_slice()).collect::<Vec<_>>(),
    ));
    let xs: Vec<f64> = rows.iter().map(|r| r.2).collect();
    let ys: Vec<f64> = rows.iter().map(|r| r.3).collect();
    let bbox: ArrayRef = Arc::new(StructArray::new(
        bbox_fields,
        vec![
            Arc::new(Float64Array::from(xs.clone())) as ArrayRef,
            Arc::new(Float64Array::from(ys.clone())),
            Arc::new(Float64Array::from(xs.clone())),
            Arc::new(Float64Array::from(ys.clone())),
        ],
        None,
    ));

    let geo = serde_json::json!({
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {
            "geometry": {
                "encoding": "WKB",
                "geometry_types": ["Point"],
                "crs": serde_json::Value::Null,
                "bbox": [
                    xs.iter().cloned().fold(f64::INFINITY, f64::min),
                    ys.iter().cloned().fold(f64::INFINITY, f64::min),
                    xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                    ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
                ],
                "covering": {"bbox": {
                    "xmin": ["bbox", "xmin"],
                    "ymin": ["bbox", "ymin"],
                    "xmax": ["bbox", "xmax"],
                    "ymax": ["bbox", "ymax"],
                }},
            }
        }
    });
    let batch = RecordBatch::try_new(schema.clone(), vec![ids, weights, geometry, bbox])
        .expect("point batch");

    let props = WriterProperties::builder()
        // 20 row groups over 3,600 points, matching the polygon grid's shape.
        .set_max_row_group_row_count(Some(180))
        // The `geo` key goes into the parquet footer explicitly rather than
        // by way of the arrow schema's metadata: whether arrow-rs propagates
        // schema metadata into the file's key-value metadata is its business,
        // and a fixture whose covering silently stops being declared would
        // turn this test into a no-op that still says `ok`. (The
        // `expect_row_group_pruning` assertion is the backstop, and it caught
        // exactly that.)
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
            "geo".to_string(),
            geo.to_string(),
        )]))
        .build();
    let file = std::fs::File::create(path).expect("create point fixture");
    let mut w = ArrowWriter::try_new(file, schema, Some(props)).expect("arrow writer");
    w.write(&batch).expect("write points");
    w.close().expect("close point fixture");
}

/// Madagascar admin-4: real, messy polygons, and seams that fall inside them.
///
/// One row group and no `geo` metadata, so nothing prunes here — this is the
/// *geometry* half of the oracle. The pruning half is below.
fn madagascar(shards: usize) -> Option<(PathBuf, Build)> {
    let input = fixture::realdata("fieldmaps-madagascar-adm4.parquet")?;
    Some((
        input,
        Build {
            min_zoom: 0,
            max_zoom: 9,
            // Madagascar covers a handful of z6 tiles, so every shard gets
            // real work and the seams land in real geometry.
            pivot: 6,
            shards,
            expect_row_group_pruning: false,
            flavor: Flavor::NoCoalesce,
            coarse_level_ceiling: false,
        },
    ))
}

/// The world-spanning grid, the fixture whose row groups actually prune.
fn grid() -> PathBuf {
    let input = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/sharding-grid.parquet");
    assert!(
        input.exists(),
        "missing {}; regenerate it with `uv run python scripts/create_sharding_fixture.py`",
        input.display()
    );
    input
}

/// The pruning build, at one flavor and one fleet size.
fn grid_build(shards: usize, flavor: Flavor) -> Build {
    Build {
        min_zoom: 0,
        max_zoom: 5,
        pivot: 3,
        shards,
        expect_row_group_pruning: true,
        flavor,
        coarse_level_ceiling: false,
    }
}

/// Two shards: the smallest fleet with a seam in it.
#[test]
fn sharded_build_matches_monolithic_with_two_shards() {
    let Some((input, build)) = madagascar(2) else {
        return;
    };
    assert_sharded_build_matches_monolithic(&input, build);
}

/// Three shards: two seams, and at least one shard with a neighbour on both
/// sides — the case a two-shard test cannot reach.
#[test]
fn sharded_build_matches_monolithic_with_three_shards() {
    let Some((input, build)) = madagascar(3) else {
        return;
    };
    assert_sharded_build_matches_monolithic(&input, build);
}

/// #541, N = 2: the coarse job caps pass 2 at the pivot.
///
/// Same fleet, same merge, same bar — but the coarse job now builds only the
/// levels it exports. Madagascar is the right fixture for it: real polygons
/// whose vertices actually move under the ladder cascade, so a fold that
/// dropped the unmaterialized fine steps would shift geometry at every coarse
/// zoom and die on the byte comparison.
#[test]
fn level_capped_coarse_job_matches_monolithic_with_two_shards() {
    let Some((input, build)) = madagascar(2) else {
        return;
    };
    assert_sharded_build_matches_monolithic(&input, capped(build));
}

/// #541, N = 3: two seams and a level-capped coarse job.
#[test]
fn level_capped_coarse_job_matches_monolithic_with_three_shards() {
    let Some((input, build)) = madagascar(3) else {
        return;
    };
    assert_sharded_build_matches_monolithic(&input, capped(build));
}

/// #541 where the shards also **prune**: the capped coarse job and the plan
/// re-addressing have to hold at the same time, under every knob's default.
#[test]
fn level_capped_coarse_job_matches_monolithic_when_shards_prune() {
    assert_sharded_build_matches_monolithic(&grid(), capped(grid_build(3, Flavor::Defaults)));
}

/// The **pruning** half of the oracle: a world-spanning grid whose 20 row
/// groups carry GeoParquet 1.1 covering statistics.
///
/// Here every shard reads a strict subset of the input, so the convert plan's
/// row-indexed winner table no longer lines up with the stream the shard sees
/// and has to be re-addressed onto it. That re-addressing is the single most
/// delicate step in a sharded build — get it wrong by one row group and every
/// feature after the gap enters at the wrong level, producing an archive that
/// is plausible, complete, and quietly wrong. Byte parity against the
/// monolithic export is what catches it.
#[test]
fn sharded_build_matches_monolithic_when_shards_prune_row_groups() {
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/streaming/sharding-grid.parquet");
    assert!(
        input.exists(),
        "missing {}; regenerate it with `uv run python scripts/create_sharding_fixture.py`",
        input.display()
    );
    for shards in [2usize, 3, 4] {
        assert_sharded_build_matches_monolithic(&input, grid_build(shards, Flavor::NoCoalesce));
    }
}

/// The same pruning build with **every knob left at its default** — which
/// means `coalesce_lines` ON, the way an operator actually runs it.
///
/// A polygon dataset produces no chains, so the plan's coalesce section is
/// present and EMPTY. The re-addressing used to assert that section absent,
/// which held in release (assertions compiled out) and panicked in every
/// debug build of the ordinary workflow. Byte parity under defaults is what
/// proves the fixed predicate is the right one rather than merely a quieter
/// one.
#[test]
fn sharded_build_matches_monolithic_under_default_options() {
    assert_sharded_build_matches_monolithic(&grid(), grid_build(3, Flavor::Defaults));
}

/// `--collapse-square` puts CARRIER rows in the plan: per level, the rows of
/// polygons too small to survive there that must still be emitted as a
/// placeholder square. They are row-indexed, so a shard has to re-key them
/// and drop the ones it does not read — the same run map as the winner table,
/// but a different code path over it.
#[test]
fn sharded_build_matches_monolithic_with_tiny_polygon_carriers() {
    assert_sharded_build_matches_monolithic(&grid(), grid_build(3, Flavor::Carriers));
}

/// `--cluster` + `--accumulate-attribute` on points puts CLUSTER TABLES in
/// the plan: a per-level map from source row to `(point_count, aggregates)`.
/// The re-addressing re-keys that map, and a key belonging to a row the shard
/// does not read is dropped.
///
/// This is the last of the four row-indexed sections, and the only one whose
/// entries are sparse — so an off-by-one here does not shift everything by a
/// row (loud), it silently attaches the wrong `point_count` to the wrong
/// winner (quiet, and visible only in the tile bytes).
#[test]
fn sharded_build_matches_monolithic_with_clustered_points() {
    let dir = tempfile::tempdir().expect("tempdir");
    let input = dir.path().join("points.parquet");
    write_point_grid(&input);
    assert_sharded_build_matches_monolithic(&input, grid_build(3, Flavor::ClusteredPoints));
}

/// Short connected line chains plus a few long lines, written at test time.
///
/// Every chain is 30 end-to-end ~111 m segments: fine levels merge each into
/// one feature with `coalesced_count = 30`, while coarse levels drop or keep
/// the chain without merging. The long lines keep the coarse levels
/// populated.
fn write_line_chains(path: &Path) {
    use std::sync::Arc;

    use arrow_array::{ArrayRef, BinaryArray, Int64Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;

    let linestring = |pts: &[(f64, f64)]| -> Vec<u8> {
        let mut out = Vec::with_capacity(9 + 16 * pts.len());
        out.push(1u8); // little endian
        out.extend_from_slice(&2u32.to_le_bytes()); // LineString
        out.extend_from_slice(&(pts.len() as u32).to_le_bytes());
        for &(x, y) in pts {
            out.extend_from_slice(&x.to_le_bytes());
            out.extend_from_slice(&y.to_le_bytes());
        }
        out
    };
    let mut geoms: Vec<Vec<u8>> = Vec::new();
    for c in 0..6 {
        let (x0, y0) = (-50.0 + c as f64 * 17.0, -20.0 + c as f64 * 8.0);
        for k in 0..30 {
            let x = x0 + k as f64 * 0.001;
            geoms.push(linestring(&[(x, y0), (x + 0.001, y0)]));
        }
    }
    for l in 0..4 {
        let y = -60.0 + l as f64 * 35.0;
        geoms.push(linestring(&[(-120.0, y), (-60.0, y + 5.0), (0.0, y - 3.0)]));
    }

    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("geometry", DataType::Binary, false),
    ]));
    let ids: ArrayRef = Arc::new(Int64Array::from(
        (0..geoms.len() as i64).collect::<Vec<_>>(),
    ));
    let geometry: ArrayRef = Arc::new(BinaryArray::from(
        geoms.iter().map(|g| g.as_slice()).collect::<Vec<_>>(),
    ));
    let geo = serde_json::json!({
        "version": "1.1.0",
        "primary_column": "geometry",
        "columns": {"geometry": {
            "encoding": "WKB",
            "geometry_types": ["LineString"],
            "crs": serde_json::Value::Null,
        }}
    });
    let batch = RecordBatch::try_new(schema.clone(), vec![ids, geometry]).expect("line batch");
    let props = WriterProperties::builder()
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new(
            "geo".to_string(),
            geo.to_string(),
        )]))
        .build();
    let file = std::fs::File::create(path).expect("create line fixture");
    let mut w = ArrowWriter::try_new(file, schema, Some(props)).expect("arrow writer");
    w.write(&batch).expect("write lines");
    w.close().expect("close line fixture");
}

/// #541 review (S1): the level-capped coarse job over **coalesced line
/// chains**, coalescing on (the default).
///
/// Only the coarse half: a data shard refuses a plan that carries coalesced
/// chains (see `shard_with_coalesced_lines_in_the_plan_is_refused`), so no
/// full fleet can be built over this input. But the coarse job itself runs
/// with chains, and its tiles must still match the monolithic build's at
/// every zoom it owns. They did not: the export decided whether to publish
/// `coalesced_count` from the file's own row-group statistics, and the
/// chains here merge only finer than the pivot — so the capped file's
/// counter never left 1, the column was withheld, and every coarse tile
/// differed from the monolithic one.
#[test]
fn level_capped_coarse_job_matches_monolithic_on_coalesced_line_chains() {
    use tylertoo_core::overview::reader::OverviewReader;

    const PIVOT: u8 = 7;
    let dir = tempfile::tempdir().expect("tempdir");
    let dir = dir.path();
    let input = dir.join("chains.parquet");
    write_line_chains(&input);
    let build = Build {
        min_zoom: 1,
        max_zoom: 12,
        pivot: PIVOT,
        shards: 1,
        expect_row_group_pruning: false,
        flavor: Flavor::Defaults,
        coarse_level_ceiling: true,
    };
    let opts = convert_options(build);
    assert!(opts.coalesce_lines, "the point is coalescing ON");

    let mono_overview = dir.join("mono.parquet");
    let full_plan = dir.join("full.plan");
    convert_to_overviews(
        &input,
        &mono_overview,
        &ConvertOptions {
            save_plan: Some(full_plan.clone()),
            ..opts
        },
    )
    .expect("monolithic convert");
    let coarse_overview = level_capped_coarse_overview(&input, dir, build, &full_plan);

    // The premise: the two files' own counters disagree, so a publish
    // decision read from them would too.
    let max = |p: &Path| {
        OverviewReader::open(p)
            .expect("open overview")
            .int_column_max("coalesced_count")
    };
    assert!(
        max(&mono_overview).is_some_and(|m| m > 1),
        "the chains must merge somewhere in the monolithic build"
    );
    assert_eq!(
        max(&coarse_overview),
        Some(1),
        "the chains must merge only past the pivot, or this oracle proves nothing"
    );

    let mono = dir.join("mono.pmtiles");
    export_pmtiles(
        &mono_overview,
        &mono,
        &ExportOptions {
            min_zoom: Some(build.min_zoom),
            ..export_options()
        },
    )
    .expect("monolithic export");
    let coarse = dir.join("coarse.pmtiles");
    export_pmtiles(
        &coarse_overview,
        &coarse,
        &ExportOptions {
            zoom_ceiling: Some(PIVOT - 1),
            min_zoom: Some(build.min_zoom),
            ..export_options()
        },
    )
    .expect("coarse export");

    let want: BTreeMap<u8, BTreeMap<u64, Vec<u8>>> = tiles_of(&mono)
        .into_iter()
        .filter(|(z, _)| *z < PIVOT)
        .collect();
    let got = tiles_of(&coarse);
    assert!(
        want.values().map(BTreeMap::len).sum::<usize>() > 0,
        "the monolithic coarse zooms are empty; the oracle would prove nothing"
    );
    assert_eq!(
        got.keys().collect::<Vec<_>>(),
        want.keys().collect::<Vec<_>>(),
        "the coarse job must hold exactly the monolithic coarse zooms"
    );
    for (z, tiles) in &want {
        assert_eq!(
            &got[z], tiles,
            "z{z}: the capped coarse job's tiles differ from the monolithic build's"
        );
    }
}
