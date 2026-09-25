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
use tylertoo_core::overview::convert::{convert_to_overviews, ConvertOptions, LevelPlan};
use tylertoo_core::overview::export::{export_pmtiles, ExportOptions};
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
}

fn convert_options(build: Build) -> ConvertOptions {
    ConvertOptions {
        levels: LevelPlan::ZoomRange {
            min_zoom: build.min_zoom,
            max_zoom: build.max_zoom,
        },
        // A chain is a new geometry spanning every row it merged, which no
        // single row group's bbox bounds — so `--shard` refuses a plan that
        // carries one. The fixture is polygons and would produce none anyway;
        // turning it off keeps the oracle honest about what it is testing.
        coalesce_lines: false,
        ..Default::default()
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

/// The oracle.
fn assert_sharded_build_matches_monolithic(input: &Path, build: Build) {
    let Build {
        min_zoom,
        max_zoom: _,
        pivot,
        shards,
        expect_row_group_pruning,
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
        let b = shard_plan.range(i).expect("range").bounds();
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
    // and exports the zooms below the pivot. Its overview file is also, by
    // construction, exactly the monolithic one — same input, same options — so
    // the control export below reuses it rather than converting twice.
    let convert_plan = dir.join("convert.plan");
    let overview = dir.join("coarse.parquet");
    convert_to_overviews(
        input,
        &overview,
        &ConvertOptions {
            save_plan: Some(convert_plan.clone()),
            ..convert_options(build)
        },
    )
    .expect("coarse convert");

    let coarse = dir.join("coarse.pmtiles");
    export_pmtiles(
        &overview,
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
        },
    ))
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
        assert_sharded_build_matches_monolithic(
            &input,
            Build {
                min_zoom: 0,
                max_zoom: 5,
                pivot: 3,
                shards,
                expect_row_group_pruning: true,
            },
        );
    }
}
