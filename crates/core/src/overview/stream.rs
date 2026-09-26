//! Two-pass bounded-memory streaming overview conversion (task V4 / H3).
//!
//! The in-memory pipeline in [`super::convert`] materializes the entire input
//! table, every decoded geometry, and a cloned geometry set per level — `O(N)`
//! memory (Moldova, 632k polygons: 5.44 GB peak RSS). This module implements
//! the same conversion in bounded memory:
//!
//! - **Pass 1** streams the input once (row batches of
//!   [`ConvertOptions::read_batch_size`] rows, geometry + ranking columns
//!   only): per feature it keeps only the bbox, [`FeatureKind`], and sort key
//!   — a small [`AssignFeature`] — plus the incremental state for Q1 ranking
//!   auto-detection. [`assign_levels_bounded`] and [`apply_density_budget`] then run
//!   over those (the assign engine's own state is `O(occupied cells)` per
//!   level), producing the **winner table**: one `min_level` byte per feature.
//! - **Pass 2** re-reads the (seekable) input once **per level**, coarse →
//!   fine: each batch is filtered against the winner table, simplified for the
//!   level (non-canonical duplicating levels only), and handed straight to the
//!   [`OverviewWriter`]. Nothing is retained across batches.
//!
//! Peak memory is `O(read batch + winner tables)`: the winner table is 1 byte
//! per feature; pass 1 additionally holds the `AssignFeature` vector (64
//! bytes/feature, [`super::convert::PASS1_BYTES_PER_ROW`]) from the scan
//! through the assignment, plus transient per-row vectors during the scan
//! (ranking-key candidates at 16 bytes/row each, and — when those options are
//! on — accumulate/ladder values, polygon areas, coalesce line geometries),
//! all freed before pass 2. Residual `O(N)` state is therefore ≥ 64 bytes per
//! input feature (typically ~64–100) — for 632k features, a few tens of MB,
//! far below the geometry payload the in-memory path holds, but tens of GiB
//! at billion-row scale (#543: preflighted from the footers before pass 1).
//!
//! Hilbert order: input order is preserved within each level (the documented
//! gpio-sorted input contract, spec §4.3), exactly as in the in-memory path —
//! no in-memory per-level sort exists in either path.
//!
//! # Behavior parity with the in-memory path
//!
//! Level assignments, density-budget cuts, ranking resolution (explicit /
//! auto-detected / fallback), footer metadata, and per-level row values are
//! identical (tested in `convert::tests`). One documented divergence: the
//! in-memory path omits (and renumbers past) a level whose *simplified*
//! output is empty, whereas this path decides level omission from the winner
//! table before simplification. The two only differ when **every** winner of
//! a level degenerates during simplification — rare under the default knobs
//! (the assign visibility gates, 2 × GSD, are stricter than the simplify
//! drop gate at 1 × GSD) but real on dirty data (#211: a sliver with a huge
//! bbox passes the gate, then collapses). When it happens the writer skips
//! the level ([`LevelWriteOutcome::SkippedEmpty`]) and this driver records it
//! in [`ConvertReport::skipped_empty_levels`], exactly like a plan-time
//! omission — the two pipelines converge on the same output pyramid.
//!
//! [`LevelWriteOutcome::SkippedEmpty`]: super::writer::LevelWriteOutcome::SkippedEmpty
//! [`ConvertReport::skipped_empty_levels`]: super::convert::ConvertReport::skipped_empty_levels

use std::cell::Cell;
use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use arrow_select::take::take;
use geo::{Area, Geometry};
use geoarrow::array::from_arrow_array;
use rayon::prelude::*;

use crate::batch_processor::{extract_geometries_from_array, extract_geometries_opt_from_array};
use crate::input_set::{ConvertSource, ReadPlan, RowGroupSelection};

use super::accumulate::{is_carrier, level_accumulates, tiny_polygon_carriers, AccumulateLevel};
use super::assign::{apply_density_budget, assign_levels_bounded, AssignFeature, FeatureKind};
use super::cluster::{ClusterEntry, ClusterTables};
use super::coalesce::CoalesceInput;
use super::convert::{
    append_coalesced_count_field, append_point_count_field, apply_cluster_columns,
    apply_coalesced_count, build_generalization, build_level_batch, build_level_coalesce_table,
    build_source_schema, class_ranking_provenance, coalesce_effective, coalesce_level_chains,
    coalesce_table_merged, count_vertices, encode_concurrency_for, extract_class_ranks,
    extract_numeric_values, extract_sort_keys, fill_level_bytes, find_geometry_column,
    mixed_geometry_field, overture_road_ranking, record_coalesce_merged, record_level_outcome,
    resolve_read_workers, resolve_reserved_column_collisions, scan_feature,
    validate_cluster_schema, validate_coalesce_schema, warn_plan_skipped_levels, BboxTallies,
    ClassRanking, CoalesceTable, ConvertError, ConvertOptions, ConvertReport, GroupInterner,
    LevelReport, SkippedLevelReport, KNOWN_ROAD_CLASSES, ROAD_VOCAB_MIN_DISTINCT,
};
use super::level::{Crs, Mode, RankingProvenance};
use super::pipe::scoped_pipe;
use super::pipeline;
use super::pipeline::{read_in_order, ReadFlow, ReadTuning};
use super::plan_state::{ConvertPlan, Fingerprint, PlanTotals, SelectionRule};
use super::simplify::{
    carrier_square, full_resolution_fallback_count, simplify_cascade, simplify_step,
    validation_skip_count, CascadeFold, CascadeStep, CollapseMode, FoldStep, Representation,
    Simplified, SimplifyOptions,
};
use super::writer::{LevelSpec, LevelWriteOutcome, OverviewWriter, OverviewWriterOptions};

/// Row-indexed winner-table sentinel for rows with no feature (null, empty,
/// or non-finite geometry — skipped in pass 1). It matches no level in either
/// mode: [`super::convert::MAX_LEVELS`] caps the plan at 255 levels, so the
/// finest level index is at most 254.
pub(super) const UNASSIGNED_LEVEL: u8 = u8::MAX;

/// A level actually emitted to the output (levels with zero winners are
/// omitted and renumbered, spec §7.3, matching the in-memory path).
struct EmitLevel {
    /// Index in the *resolved* level plan (drives winner-table membership).
    orig: u8,
    gsd: f64,
    zoom: Option<u8>,
    /// Winner count — the writer's `level_row_hint` for row-group sizing.
    hint: usize,
}

/// Pass-2 execution strategy. Both produce byte-identical output; `Serial` is
/// the pre-#213 per-level-re-read reference, retained for differential testing.
#[derive(Clone, Copy)]
pub(crate) enum Pass2Strategy {
    /// One in-order re-read per level (the reference path).
    #[cfg_attr(not(test), allow(dead_code))]
    Serial,
    /// Single-read pipelined engine ([`super::pipeline`]); the production path.
    Pipelined,
}

/// Streaming counterpart of [`super::convert::convert_to_overviews`], with an
/// explicit pass-2 [`Pass2Strategy`] (production uses `Pipelined`; tests pin
/// `Serial` to assert the pipelined engine is equivalent).
/// Info-level summary of RDP candidates whose validity check was skipped by
/// the vertex cap during this conversion (#242). `skips_before` is the
/// process-wide counter snapshot taken before pass 2.
fn log_validation_skips(skips_before: u64) {
    let skips = validation_skip_count() - skips_before;
    if skips > 0 {
        log::info!(
            "[convert] {skips} oversized RDP candidate(s) skipped exact \
             validity checking and were assumed valid (#242; geometry \
             validity is not an overviews conformance requirement)"
        );
    }
}

/// Pass 0 (#286/#287): stage the selected row groups to local disk up front so
/// the two passes below read from the spill, not the network.
///
/// A row group's column chunks are a contiguous span, so each is fetched as ONE
/// coalesced range request (several in flight per part) and spilled. This
/// removes the per-column-chunk serial re-fetch (#287) and the cold pass-2
/// re-fetch of the property columns pass 1's projection skips (#286).
/// Best-effort and no-op for local input: a staging error is logged and the
/// passes fall back to the reader's lazy network path (the same bytes, just
/// uncoalesced), so staging never regresses correctness.
fn stage_input_pass0(
    source: &ConvertSource,
    selected_row_groups: Option<&RowGroupSelection>,
    row_groups_read: usize,
) {
    if !source.is_remote() {
        return;
    }
    let t_stage = Instant::now();
    match source.stage_selected(selected_row_groups) {
        Ok(()) => log::info!(
            "[convert] staged {row_groups_read} selected row group(s) to local \
             disk in {:.1}s",
            t_stage.elapsed().as_secs_f64()
        ),
        Err(e) => log::warn!(
            "[convert] input staging failed ({e}); passes will read over the \
             network (uncoalesced)"
        ),
    }
}

/// Resolve an in-flight depth (auto-sizing from available cores when the
/// caller left it at [`super::convert::IN_FLIGHT_BATCHES_AUTO`]) and log the
/// chosen depth alongside the detected core count, so core utilization is
/// observable rather than a mystery (#264; shared by pass 1 and pass 2 as of
/// #460, since both now run a dedicated reader thread ahead of a bounded
/// channel — `phase` names which one in the log line, e.g. `"pass 1"`).
fn resolve_and_log_in_flight_batches(phase: &str, requested: usize) -> usize {
    let in_flight = super::convert::resolve_in_flight_batches(requested);
    log::info!(
        "[convert] {phase} parallelism: {in_flight} read batch(es) in flight ({} core(s) detected)",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(0)
    );
    in_flight
}

/// Current process resident-set size (RSS) in MiB, if the platform exposes it.
fn current_rss_mib() -> Option<f64> {
    memory_stats::memory_stats().map(|m| m.physical_mem as f64 / (1024.0 * 1024.0))
}

/// Log the process RSS at a convert-phase boundary (#295 instrumentation) and
/// fold it into the running peak. Silent when the platform can't report RSS.
///
/// These `[rss] <phase>` lines pinpoint which phase dominates peak memory —
/// pass-1 winner tables (O(dataset)) vs the pass-2 output sink (bounded by the
/// #294 auto profile) — and validate the auto backing choice on real runs.
fn log_phase_rss(phase: &str, peak_mib: &mut Option<f64>) {
    if let Some(rss) = sample_rss_peak(peak_mib) {
        log::info!("[rss] {phase}: {rss:.0} MiB");
    }
}

/// Sample the current RSS and fold it into the running `peak_mib`, silently;
/// returns the sample. The shared core of [`log_phase_rss`], also used by the
/// export profile (#535), which samples at its phase boundaries without adding
/// `[rss]` log lines of its own.
pub(super) fn sample_rss_peak(peak_mib: &mut Option<f64>) -> Option<f64> {
    let rss = current_rss_mib()?;
    if peak_mib.is_none_or(|p| rss > p) {
        *peak_mib = Some(rss);
    }
    Some(rss)
}

/// Split the resolved level plan into emitted levels (have winners) and skipped
/// levels (no winners → omitted per §7.3 / #211 auto-clamp). `counts[l]` is the
/// winner count for planned level `l`.
fn partition_emitted_levels(
    level_specs: &[(f64, Option<u8>)],
    counts: &[usize],
) -> (Vec<EmitLevel>, Vec<SkippedLevelReport>) {
    let skipped = level_specs
        .iter()
        .enumerate()
        .filter(|&(l, _)| counts[l] == 0)
        .map(|(l, &(gsd, zoom))| SkippedLevelReport {
            planned_level: l,
            gsd,
            zoom,
        })
        .collect();
    let emitted = level_specs
        .iter()
        .enumerate()
        .filter(|&(l, _)| counts[l] > 0)
        .map(|(l, &(gsd, zoom))| EmitLevel {
            orig: l as u8,
            gsd,
            zoom,
            hint: counts[l],
        })
        .collect();
    (emitted, skipped)
}

/// How many of `planned` a convert-side zoom ceiling (#541) materializes.
///
/// Levels run coarse→fine with strictly ascending zooms, so a ceiling always
/// keeps a **prefix** — which is what lets pass 2 take `&planned[..kept]` and
/// leave every index (cluster tables, carriers, cascade chains) alone. `None`
/// keeps everything; a level with no zoom (an explicit GSD ladder) is kept
/// too, since nothing proves it is above the ceiling — `validate_options`
/// refuses that pairing up front, so this is belt-and-braces.
fn levels_at_or_above_ceiling(planned: &[EmitLevel], ceiling: Option<u8>) -> usize {
    let Some(ceiling) = ceiling else {
        return planned.len();
    };
    planned
        .iter()
        .take_while(|e| e.zoom.is_none_or(|z| z <= ceiling))
        .count()
}

/// Build the three writer schemas (identical to the in-memory path):
/// `source` (base), `cluster` (+ `point_count` when clustering, Q4), and `out`
/// (+ `coalesced_count` when coalescing, Q3). All three are needed downstream,
/// so they are returned together.
pub(super) fn build_level_schemas(
    input_schema: &Schema,
    geom_idx: usize,
    geom_name: &str,
    crs: Crs,
    options: &ConvertOptions,
) -> (Schema, Schema, Schema) {
    // The output geometry field must inherit the INPUT field's geoarrow
    // extension metadata — above all its CRS. Dropping it made the overview
    // claim the GeoParquet default (OGC:CRS84) for a metre file (#519).
    let geom_out_field = mixed_geometry_field(
        geom_name,
        super::convert::geometry_field_metadata(input_schema, geom_idx, crs),
    );
    let source_schema = build_source_schema(input_schema, geom_idx, geom_out_field);
    let cluster_schema = if options.cluster {
        append_point_count_field(&source_schema)
    } else {
        source_schema.clone()
    };
    let out_schema = if options.coalesce_lines {
        append_coalesced_count_field(&cluster_schema)
    } else {
        cluster_schema.clone()
    };
    (source_schema, cluster_schema, out_schema)
}

/// Build the writer options both convert paths use: the shared knobs plus the
/// generalization provenance recorded in the footer (§3.5), and the #507
/// row-group-ceiling preflight — computed from `level_row_counts` (the same
/// per-level winner hints the writer sizes row groups from) BEFORE pass 2
/// ever opens the output file, so a projected overflow is caught (and the
/// cap auto-scaled) in milliseconds instead of after hours of writing.
pub(super) fn build_writer_options(
    writer_levels: Vec<LevelSpec>,
    emitted_gsds: &[f64],
    level_row_counts: &[usize],
    crs: Crs,
    ranking_provenance: RankingProvenance,
    renames: &[(String, String)],
    options: &ConvertOptions,
) -> Result<OverviewWriterOptions, ConvertError> {
    build_writer_options_with_ceiling(
        writer_levels,
        emitted_gsds,
        level_row_counts,
        crs,
        ranking_provenance,
        renames,
        options,
        super::writer::SAFE_ROW_GROUP_CEILING,
    )
}

/// [`build_writer_options`], parameterized on the safety ceiling (#507 test
/// seam): production always calls it via `build_writer_options` with
/// [`super::writer::SAFE_ROW_GROUP_CEILING`]; tests pass a tiny ceiling to
/// exercise the auto-scale / unreachable-error paths without constructing
/// tens of thousands of row groups.
#[allow(clippy::too_many_arguments)]
fn build_writer_options_with_ceiling(
    writer_levels: Vec<LevelSpec>,
    emitted_gsds: &[f64],
    level_row_counts: &[usize],
    crs: Crs,
    ranking_provenance: RankingProvenance,
    renames: &[(String, String)],
    options: &ConvertOptions,
    ceiling: usize,
) -> Result<OverviewWriterOptions, ConvertError> {
    let mut writer_opts = OverviewWriterOptions::new(options.mode, writer_levels);
    writer_opts.max_row_group_size = options.max_row_group_size;
    writer_opts.row_group_size_policy = options.row_group_size_policy;
    writer_opts.full_column_stats = options.full_column_stats;
    writer_opts.cogp_compat_key = options.cogp_compat_key;
    writer_opts.encode_concurrency = encode_concurrency_for(options.profile);
    writer_opts.generalization = Some(build_generalization(
        emitted_gsds,
        crs,
        options,
        ranking_provenance,
        renames,
    ));

    // #507: preflight parquet's per-file row-group ceiling. Mirrors the
    // writer's own per-level split arithmetic (`projected_row_groups`) exactly,
    // but is fed `level_row_counts` — pass 1's PRE-simplification winner hints
    // — so the result is a conservative UPPER BOUND on the row-group count
    // `write_level` will actually produce, not a prediction of it. (Measured:
    // 807 projected vs 776 written on a fixture where simplification collapsed
    // winners. Over-estimating is the safe direction: it can only auto-scale
    // the cap sooner than strictly needed, never later.)
    let level_zooms: Vec<Option<u8>> = writer_opts.levels.iter().map(|l| l.zoom).collect();
    let finest_zoom = writer_opts.levels.last().and_then(|l| l.zoom);
    match super::writer::autoscale_cap(
        level_row_counts,
        writer_opts.max_row_group_size,
        writer_opts.row_group_size_policy,
        &level_zooms,
        finest_zoom,
        ceiling,
    ) {
        Some(cap) if cap > writer_opts.max_row_group_size => {
            let projected = super::writer::projected_row_groups(
                level_row_counts,
                writer_opts.max_row_group_size,
                writer_opts.row_group_size_policy,
                &level_zooms,
                finest_zoom,
            );
            log::warn!(
                "[convert] projected output needs up to {projected} row groups at \
                 --row-group-size {old} — over the {ceiling}-row-group preflight ceiling \
                 (parquet's hard limit is 32,768 row groups per file); auto-scaling \
                 --row-group-size to {cap} to fit. Larger row groups mean proportionally \
                 more memory held per row group while writing. Pass --row-group-size {cap} \
                 explicitly to silence this warning.",
                old = writer_opts.max_row_group_size,
            );
            writer_opts.max_row_group_size = cap;
        }
        Some(_) => {}
        None => {
            return Err(ConvertError::RowGroupCeilingUnreachable {
                // PLANNED levels: `level_row_counts` are pre-simplification
                // hints, so a level whose winners all collapse still counts
                // here. An upper bound is the right side to err on for a
                // preflight, and the message says "planned" rather than
                // claiming these are the levels that get written.
                levels: level_row_counts.iter().filter(|&&n| n > 0).count(),
                ceiling,
            });
        }
    }

    Ok(writer_opts)
}

/// Combined per-part footer-statistics row-group selection for the streaming
/// path: bbox covering pruning (#102) intersected with attribute-filter
/// statistics pushdown (#315) and, on a data shard, with the shard's own tile
/// range (#498). `None` when none of the three is active.
///
/// The shard term is the one that is *not* fingerprinted: `--bbox` and
/// `--filter` change which features exist and so change the assignment, but a
/// shard reads a subset of the same dataset under the same assignment. That
/// asymmetry is why the plan's fingerprint compares the selection as a subset
/// relation in shard mode instead of an equality, and why the plan's
/// row-indexed tables are then re-addressed onto the subset rather than
/// re-derived over it.
fn select_row_groups_streaming(
    source: &ConvertSource,
    bbox_units: Option<&[f64; 4]>,
    filter: Option<&super::filter::BoundFilter>,
    shard: Option<&crate::shard::TileRange>,
    crs: Crs,
) -> Result<Option<RowGroupSelection>, ConvertError> {
    let bbox_selection: Option<RowGroupSelection> = match bbox_units {
        Some(bb) => Some(source.select_row_groups(bb)?),
        None => None,
    };
    let filter_selection: Option<RowGroupSelection> = match filter {
        Some(f) => Some(source.select_row_groups_matching(f)?),
        None => None,
    };
    let shard_selection: Option<RowGroupSelection> = match shard {
        Some(range) => {
            let b = range.read_bounds();
            let units = super::convert::bbox_to_crs_units(
                &[b.lng_min, b.lat_min, b.lng_max, b.lat_max],
                crs,
            );
            Some(source.select_row_groups(&units)?)
        }
        None => None,
    };
    Ok([bbox_selection, filter_selection, shard_selection]
        .into_iter()
        .flatten()
        .reduce(|a, b| a.intersect(&b)))
}

/// Unsigned area of a polygonal geometry in CRS units², 0 for anything else
/// (the accumulator's per-feature input, #384).
fn polygon_area_f32(g: &Geometry<f64>) -> f32 {
    match g {
        Geometry::Polygon(p) => p.unsigned_area() as f32,
        Geometry::MultiPolygon(mp) => mp.unsigned_area() as f32,
        _ => 0.0,
    }
}

/// Run the tiny-polygon accumulator (#384) for the streaming path: per
/// planned level, the sorted row indices of its carriers; empty per level
/// unless the accumulator applies there.
fn streaming_carriers(
    options: &ConvertOptions,
    features: &[AssignFeature],
    feat_min_levels: &[u8],
    areas: Vec<f32>,
    level_gsds: &[f64],
    level_reprs: &[Representation],
    crs: Crs,
) -> Vec<Vec<usize>> {
    let finest_planned = level_gsds.len().saturating_sub(1);
    let acc_levels: Vec<AccumulateLevel> = level_gsds
        .iter()
        .enumerate()
        .map(|(l, &gsd)| AccumulateLevel {
            gsd_meters: gsd,
            enabled: l != finest_planned
                && accumulator_enabled(options)
                && level_accumulates(options.simplify.collapse, level_reprs[l]),
        })
        .collect();
    if !acc_levels.iter().any(|l| l.enabled) {
        return vec![Vec::new(); level_gsds.len()];
    }
    let t = Instant::now();
    let carriers = tiny_polygon_carriers(
        features,
        feat_min_levels,
        &areas,
        &acc_levels,
        crs,
        options.simplify.factor,
    );
    let total: usize = carriers.iter().map(Vec::len).sum();
    log::info!(
        "[convert] tiny-polygon accumulator: {total} placeholder square(s) across {} \
         level(s) stand in for the polygons those levels dropped ({:.2}s)",
        acc_levels.iter().filter(|l| l.enabled).count(),
        t.elapsed().as_secs_f64()
    );
    carriers
}

/// Whether the tiny-polygon accumulator (#384) is in play for this run:
/// duplicating mode (a carrier is a second appearance of a feature, which
/// partitioning's feature-once contract cannot represent) with the square
/// disposition somewhere — globally via `--collapse-square`, or in a
/// `--representation` square band.
fn accumulator_enabled(options: &ConvertOptions) -> bool {
    matches!(options.mode, Mode::Duplicating)
        && (options.simplify.collapse == CollapseMode::Square
            || options
                .representation
                .iter()
                .any(|b| b.repr == Representation::Square))
}

/// Prebuild every non-verbatim level's coalesce chain table.
///
/// The single read fans each batch out to all levels at once, so every level's
/// table has to exist before the read starts. Deterministic and keyed by rep
/// row, so the result is byte-identical to the former per-level build.
fn build_pass2_coalesce_tables(
    coalesce_scratch: Option<&CoalesceScratch>,
    emitted: &[EmitLevel],
    finest: usize,
    crs: Crs,
    options: &ConvertOptions,
) -> Vec<Option<CoalesceTable>> {
    match coalesce_scratch {
        Some(scratch) => {
            log::info!(
                "[convert] building coalesce chain tables for {} level(s)",
                emitted.len()
            );
            let inputs = scratch.inputs();
            emitted
                .par_iter()
                .map(|e| {
                    let verbatim =
                        matches!(options.mode, Mode::Partitioning) || e.orig as usize == finest;
                    (!verbatim).then(|| {
                        build_level_coalesce_table(
                            &inputs,
                            e.orig as usize,
                            finest,
                            e.gsd,
                            crs,
                            options,
                        )
                    })
                })
                .collect()
        }
        None => std::iter::repeat_with(|| None)
            .take(emitted.len())
            .collect(),
    }
}

/// Cascading (#218): per level, the fine→coarse GSD chain from the finest
/// non-canonical level down to (and including) that level.
///
/// Chains are built from the emitted plan, so the Serial fold, the pipelined
/// incremental fold, and the in-memory path all step through the same GSD
/// sequence. The chain is empty when cascading does not apply to the level.
/// Zoom-band representation selector (#317 / #279): steps carry each
/// contributing level's representation so the fold pointifies / squarifies at
/// the right band level (see `simplify_cascade`).
fn build_cascade_chains(
    emitted: &[EmitLevel],
    finest: usize,
    duplicating: bool,
    options: &ConvertOptions,
) -> Vec<Vec<CascadeStep>> {
    let repr_of =
        |zoom: Option<u8>| super::convert::representation_for_zoom(&options.representation, zoom);
    emitted
        .iter()
        .map(|e| {
            let verbatim = matches!(options.mode, Mode::Partitioning) || e.orig as usize == finest;
            if !duplicating || verbatim || !options.simplify.cascade {
                return Vec::new();
            }
            let mut chain: Vec<CascadeStep> = emitted
                .iter()
                .filter(|f| (f.orig as usize) < finest && f.orig >= e.orig)
                .map(|f| CascadeStep {
                    gsd_meters: f.gsd,
                    repr: repr_of(f.zoom),
                })
                .collect();
            chain.reverse();
            chain
        })
        .collect()
}

/// Everything the per-level pass-2 contexts borrow from the convert driver.
///
/// `LevelStreamCtx` holds a dozen borrows into pass-1 state. Threading them
/// through as separate parameters made the builder's signature longer than its
/// body, so they travel together.
struct LevelCtxInputs<'a> {
    source_schema: &'a Schema,
    cluster_schema: &'a Schema,
    out_schema: &'a Schema,
    non_geom_cols: &'a [usize],
    geom_idx: usize,
    min_levels: &'a [u8],
    acc_cols: &'a [usize],
    kinds: Option<&'a [FeatureKind]>,
    cluster_tables: Option<&'a ClusterTables>,
    coalesce_tables: &'a [Option<CoalesceTable>],
    cascade_chains: &'a [Vec<CascadeStep>],
    /// Per planned level, the tiny-polygon accumulator's carrier rows (#384).
    carriers: &'a [Vec<usize>],
    crs: Crs,
    finest: usize,
    duplicating: bool,
}

/// Build one `LevelStreamCtx` per emitted level, in level order.
fn build_level_ctxs<'a>(
    emitted: &[EmitLevel],
    options: &'a ConvertOptions,
    inputs: &LevelCtxInputs<'a>,
) -> Vec<LevelStreamCtx<'a>> {
    let repr_of =
        |zoom: Option<u8>| super::convert::representation_for_zoom(&options.representation, zoom);
    emitted
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let verbatim =
                matches!(options.mode, Mode::Partitioning) || e.orig as usize == inputs.finest;
            LevelStreamCtx {
                source_schema: inputs.source_schema,
                cluster_schema: inputs.cluster_schema,
                out_schema: inputs.out_schema,
                non_geom_cols: inputs.non_geom_cols,
                geom_idx: inputs.geom_idx,
                min_levels: inputs.min_levels,
                orig_level: e.orig,
                duplicating: inputs.duplicating,
                verbatim,
                gsd_m: e.gsd,
                repr: repr_of(e.zoom),
                crs: inputs.crs,
                simplify: &options.simplify,
                cluster_enabled: options.cluster,
                // Canonical level: singleton clusters, columns verbatim (§2.4).
                cluster_table: inputs
                    .cluster_tables
                    .filter(|_| e.orig as usize != inputs.finest)
                    .map(|t| &t[e.orig as usize]),
                acc_cols: inputs.acc_cols,
                coalesce_enabled: options.coalesce_lines,
                kinds: inputs.kinds,
                coalesce_table: inputs.coalesce_tables[i].as_ref(),
                cascade_chain: &inputs.cascade_chains[i],
                carriers: &inputs.carriers[e.orig as usize],
            }
        })
        .collect()
}

/// Per-level `(outcome, rows, vertices, spill_bytes_written)`. `spill_bytes`
/// is 0 for every level under the `Serial` strategy (a reference/test path
/// that never spills) and for the streamed finest level (verbatim, written
/// directly — never buffered or spilled).
type LevelStat = (LevelWriteOutcome, usize, usize, u64);

/// Run pass 2 over every emitted level and return each level's [`LevelStat`],
/// in level order, plus the pipelined engine's aggregated
/// [`pipeline::Pass2EngineResult::timers`] stage split ([profile] /
/// `TYLERTOO_PROFILE_JSON` instrumentation).
///
/// The outcome distinguishes a written level from one the writer skipped
/// because every candidate collapsed during simplification (#211).
#[allow(clippy::too_many_arguments)]
fn run_pass2_levels(
    writer: &mut OverviewWriter<File>,
    ctxs: &[LevelStreamCtx<'_>],
    hints: &[usize],
    source: &ConvertSource,
    options: &ConvertOptions,
    selected_row_groups: Option<&RowGroupSelection>,
    in_flight_batches: usize,
    out_schema: &Schema,
    num_rows: usize,
    geom_bytes: u64,
    strategy: Pass2Strategy,
) -> Result<(Vec<LevelStat>, Pass2Timers), ConvertError> {
    let n = ctxs.len();
    // How pass 2 reads the input: the batch chunking (identical for every
    // worker count), the resolved reader-thread count (#494), and pass 1's
    // measured geometry weight, which is what sizes the read-ahead against
    // the memory budget.
    let read_tuning = ReadTuning {
        batch_size: options.read_batch_size.max(1),
        workers: resolve_read_workers(options.read_workers),
        avg_geom_bytes: (num_rows > 0).then(|| geom_bytes / num_rows as u64),
        profile: options.profile,
    };
    let (level_stats, engine_timers): (Vec<LevelStat>, Pass2Timers) = match strategy {
        // Reference: one in-order re-read per level (pre-#213 behavior).
        Pass2Strategy::Serial => (
            ctxs.iter()
                .enumerate()
                .map(|(i, ctx)| {
                    write_level_streaming(
                        writer,
                        i,
                        hints[i],
                        source,
                        read_tuning,
                        in_flight_batches,
                        selected_row_groups,
                        ctx,
                    )
                    // Serial is a reference/test path (pre-#213 behavior, no
                    // buffered engine); it never feeds `TYLERTOO_PROFILE_JSON`
                    // in production, so its per-level timers are discarded
                    // rather than accumulated.
                    .map(|(o, r, v, _timers)| (o, r, v, 0u64))
                })
                .collect::<Result<_, _>>()?,
            Pass2Timers::default(),
        ),
        // Production: buffer levels 0..n-1 from a single read, then stream the
        // finest (verbatim, largest) level last straight into the writer.
        Pass2Strategy::Pipelined => {
            // Streaming the last level separately exists for ONE reason: it is
            // the verbatim canonical level, far too large to buffer. Under a
            // convert-side zoom ceiling (#541) the coarse job's finest level
            // is an ordinary simplified one, so it joins the buffered set and
            // the whole job costs a single read instead of two.
            let stream_last = ctxs[n - 1].verbatim;
            let buffered = if stream_last { n - 1 } else { n };
            let buffered_rows: usize = hints[..buffered].iter().sum();
            // #305: pass 1's measured average encoded-geometry size per input
            // row sizes the RAM-vs-spill estimate (falls back to calibrated
            // constants on an empty scan). Same decision timing as before —
            // pass 1 has always completed by this point (the assign barrier).
            let avg_geom_bytes = (num_rows > 0).then(|| geom_bytes / num_rows as u64);
            let backing = pipeline::resolve_backing(
                options.profile,
                options.mode,
                buffered_rows,
                avg_geom_bytes,
            );
            log::info!(
                "[convert] pass 2: building {n} overview level(s) from a single read{}",
                if stream_last {
                    " (finest level streamed last)"
                } else {
                    ""
                }
            );
            let (mut stats, engine_timers) = if buffered > 0 {
                let result = pipeline::run_pass2_buffered(
                    writer,
                    &ctxs[..buffered],
                    &hints[..buffered],
                    source,
                    read_tuning,
                    selected_row_groups,
                    in_flight_batches,
                    backing,
                    out_schema,
                )?;
                (result.levels, result.timers)
            } else {
                (Vec::new(), Pass2Timers::default())
            };
            if stream_last {
                let (o, r, v, finest_timers) = write_level_streaming(
                    writer,
                    n - 1,
                    hints[n - 1],
                    source,
                    read_tuning,
                    in_flight_batches,
                    selected_row_groups,
                    &ctxs[n - 1],
                )?;
                // #517 S1: the finest level's own stage timers never otherwise
                // reach `engine_timers` (`run_pass2_buffered` only covers
                // levels `0..n-1`) — fold them in so `pass2.stage_secs` in the
                // `TYLERTOO_PROFILE_JSON` dump accounts for every level,
                // matching `pass2.rows` and `phase_walls.pass2`, which already
                // do.
                finest_timers.fold_into(&engine_timers);
                stats.push((o, r, v, 0u64));
            }
            (stats, engine_timers)
        }
    };
    Ok((level_stats, engine_timers))
}

/// Fold every emitted level's [`LevelStat`] into the shared bookkeeping
/// (#211): `record_level_outcome` appends a renumbered [`LevelReport`] for a
/// written level, or — for a level the writer omitted because every candidate
/// collapsed during simplification — warns and records the plan in `skipped`,
/// exactly like a plan-time omission. Returns the level reports alongside a
/// parallel `spill_bytes` vector (same push/skip pattern, so the two stay the
/// same length — the `TYLERTOO_PROFILE_JSON` per-level dump).
fn build_level_reports(
    emitted: &[EmitLevel],
    level_stats: Vec<LevelStat>,
    skipped: &mut Vec<SkippedLevelReport>,
) -> (Vec<LevelReport>, Vec<u64>) {
    let mut level_reports = Vec::with_capacity(emitted.len());
    let mut level_spill_bytes: Vec<u64> = Vec::with_capacity(emitted.len());
    for (e, (outcome, rows, vertices, spill_bytes)) in emitted.iter().zip(level_stats) {
        let reports_before = level_reports.len();
        record_level_outcome(
            outcome,
            SkippedLevelReport {
                planned_level: e.orig as usize,
                gsd: e.gsd,
                zoom: e.zoom,
            },
            e.hint,
            rows,
            vertices,
            &mut level_reports,
            skipped,
        );
        if level_reports.len() > reports_before {
            level_spill_bytes.push(spill_bytes);
        }
    }
    (level_reports, level_spill_bytes)
}

/// The resolved ranking tier: the per-row sort keys (absent for the size
/// fallback), the provenance record, and the per-row class groups that line
/// coalescing needs (present only for the class-based tiers).
type ResolvedRanking = (
    Option<Vec<Option<f64>>>,
    RankingProvenance,
    Option<Vec<u32>>,
);

/// Pick the ranking tier, in the same order and with the same logging as the
/// in-memory path.
fn resolve_ranking_tier(
    plan: RankPlan,
    explicit_keys: Vec<Option<f64>>,
    confidence_keys: Vec<Option<f64>>,
    explicit_groups: Vec<u32>,
    collect_lines: bool,
    feature_count: usize,
    point_count: usize,
) -> ResolvedRanking {
    let n = feature_count;
    let size_fallback = || {
        log::info!(
            "overview ranking: no sort key specified or auto-detected; using size + \
             deterministic-hash fallback"
        );
        RankingProvenance {
            mode: "size-fallback".to_string(),
            column: None,
            ranks: None,
            unknown_rank: None,
        }
    };

    // Resolve the tier (same order + logging as the in-memory path). The
    // third element is the all-row class-group vector for coalescing, present
    // only for the class-based tiers (matches `coalesce_group_column`).
    match plan {
        RankPlan::ExplicitSort { name, .. } => {
            log::info!("overview ranking: explicit numeric sort-key column {name:?}");
            (
                Some(explicit_keys),
                RankingProvenance {
                    mode: "explicit-sort-key".to_string(),
                    column: Some(name),
                    ranks: None,
                    unknown_rank: None,
                },
                None,
            )
        }
        RankPlan::ExplicitClass { ranking, .. } => {
            log::info!(
                "overview ranking: explicit class-ranking on column {:?} ({} named classes, unknown_rank={})",
                ranking.column,
                ranking.ranks.len(),
                ranking.unknown_rank
            );
            (
                Some(explicit_keys),
                class_ranking_provenance("class-ranking", &ranking),
                collect_lines.then_some(explicit_groups),
            )
        }
        RankPlan::Auto { roads, confidence } => {
            if let Some(cand) = roads
                .into_iter()
                .find(|c| c.found.len() >= ROAD_VOCAB_MIN_DISTINCT)
            {
                log::info!(
                    "overview ranking: auto-detected Overture road classes in column {:?}; \
                     applying built-in ranking (motorway > … > service > tail)",
                    cand.ranking.column
                );
                let prov = class_ranking_provenance("auto-overture-roads", &cand.ranking);
                (Some(cand.keys), prov, collect_lines.then_some(cand.groups))
            } else if let Some((_, col_name)) = confidence.filter(|_| n > 0 && point_count * 2 >= n)
            {
                log::info!(
                    "overview ranking: auto-detected Overture places confidence column {col_name:?} \
                     (numeric point ranking)"
                );
                (
                    Some(confidence_keys),
                    RankingProvenance {
                        mode: "auto-confidence".to_string(),
                        column: Some(col_name),
                        ranks: None,
                        unknown_rank: None,
                    },
                    None,
                )
            } else {
                (None, size_fallback(), None)
            }
        }
        RankPlan::SizeFallback => (None, size_fallback(), None),
    }
}

/// The writer and the three schemas pass 2 encodes against.
struct LevelWriter {
    writer: OverviewWriter<File>,
    source_schema: Schema,
    cluster_schema: Schema,
    out_schema: Schema,
    /// Input-schema indices of every column except the geometry column.
    non_geom_cols: Vec<usize>,
    /// `Some(cap)` when the #507 preflight raised `--row-group-size` to fit
    /// parquet's row-group ceiling — see
    /// [`ConvertReport::effective_max_row_group_size`].
    effective_max_row_group_size: Option<usize>,
}

/// #541: apply a convert-side zoom ceiling to the planned levels. Returns how
/// many of `planned` are materialized, and the ceiling when it actually
/// truncated the plan (only then is the output a partial overview). Levels
/// above the ceiling leave the #211 skipped report, and their pass-1 tables
/// are released in place.
fn apply_level_ceiling(
    planned: &[EmitLevel],
    ceiling_opt: Option<u8>,
    skipped: &mut Vec<SkippedLevelReport>,
    cluster_tables: &mut Option<ClusterTables>,
    carriers: &mut [Vec<usize>],
) -> Result<(usize, Option<u8>), ConvertError> {
    let kept = levels_at_or_above_ceiling(planned, ceiling_opt);
    // Whether the ceiling actually truncated the plan: only then is the file
    // a partial overview (and only then may an empty result be the coarse
    // job's legal "nothing here" rather than a real NoData).
    let truncated_at =
        (kept < planned.len()).then(|| ceiling_opt.expect("only a ceiling drops planned levels"));
    if kept == 0 {
        // Every planned level is finer than the ceiling (#211 auto-clamp took
        // the coarse ones). Pass 1, the assignment and any `--save-plan` are
        // done and valid; there is just nothing for THIS job to write.
        return Err(ConvertError::NothingAtOrBelowCeiling {
            ceiling: truncated_at.expect("kept == 0 < planned.len()"),
        });
    }
    if let Some(ceiling) = truncated_at {
        log::info!(
            "[convert] level ceiling z{ceiling}: materializing {kept} of {} planned \
             overview level(s) — pass 1, the level assignment and any saved plan \
             stay full-range",
            planned.len()
        );
        // A level above the ceiling was never going to be written, so it is
        // not an omission the #211 auto-clamp should report.
        skipped.retain(|s| s.zoom.is_none_or(|z| z <= ceiling));
        // #541 review: the per-level pass-1 tables are indexed by PLANNED
        // level. The levels finer than the deepest kept one are never
        // materialized, so their cluster tables and carrier lists would only
        // sit resident through pass 2. Emptied in place (not truncated) so
        // every `orig`-indexed lookup stays valid.
        let deepest = planned[kept - 1].orig as usize;
        if let Some(tables) = cluster_tables.as_mut() {
            for t in tables.iter_mut().skip(deepest + 1) {
                *t = Default::default();
            }
        }
        for c in carriers.iter_mut().skip(deepest + 1) {
            *c = Vec::new();
        }
    }
    Ok((kept, truncated_at))
}

/// Footer provenance only pass 2's setup knows, recorded when the writer is
/// created (#541 review).
struct FooterFacts {
    /// Whether any chain at any planned non-canonical level joined two or
    /// more segments (`CoalescingProvenance::merged`).
    coalesce_merged: bool,
    /// The zoom ceiling, when it truncated the level plan
    /// (`Generalization::zoom_ceiling`).
    zoom_ceiling: Option<u8>,
}

/// Whether line coalescing merged anything at ANY planned non-canonical level
/// (#379 / #541 review) — a property of the conversion, independent of which
/// levels a zoom ceiling lets this run materialize.
///
/// `built` are the chain tables for the materialized levels; when one of them
/// already merged something, that settles it. Otherwise each unmaterialized
/// level's table is built (the same `build_level_coalesce_table` a full run
/// would call), checked and dropped — short-circuiting at the first merge, so
/// the extra cost is paid only by a capped run over line data that merged
/// nothing coarse. Without it a capped coarse job and the full run would
/// disagree on whether `coalesced_count` is published, and the coarse tiles
/// would differ from the monolithic build's.
/// The materialized levels' chain tables (`planned[..kept]`; a level above
/// the ceiling never emits a row, so its table would be built and thrown
/// away), plus [`coalesce_merged_anywhere`] over the whole plan.
fn pass2_coalesce(
    scratch: Option<&CoalesceScratch>,
    planned: &[EmitLevel],
    kept: usize,
    finest: usize,
    crs: Crs,
    options: &ConvertOptions,
) -> (Vec<Option<CoalesceTable>>, bool) {
    let tables = build_pass2_coalesce_tables(scratch, &planned[..kept], finest, crs, options);
    let merged = coalesce_merged_anywhere(scratch, &tables, &planned[kept..], finest, crs, options);
    (tables, merged)
}

fn coalesce_merged_anywhere(
    scratch: Option<&CoalesceScratch>,
    built: &[Option<CoalesceTable>],
    unmaterialized: &[EmitLevel],
    finest: usize,
    crs: Crs,
    options: &ConvertOptions,
) -> bool {
    if built.iter().flatten().any(coalesce_table_merged) {
        return true;
    }
    let Some(scratch) = scratch else {
        return false;
    };
    if unmaterialized.is_empty() || matches!(options.mode, Mode::Partitioning) {
        return false;
    }
    let inputs = scratch.inputs();
    unmaterialized
        .par_iter()
        .filter(|e| e.orig as usize != finest)
        .any(|e| {
            coalesce_table_merged(&build_level_coalesce_table(
                &inputs,
                e.orig as usize,
                finest,
                e.gsd,
                crs,
                options,
            ))
        })
}

#[allow(clippy::too_many_arguments)]
fn create_level_writer(
    output_path: &Path,
    input_schema: &Schema,
    geom_idx: usize,
    geom_field: &Field,
    emitted: &[EmitLevel],
    crs: Crs,
    ranking_provenance: RankingProvenance,
    renames: &[(String, String)],
    options: &ConvertOptions,
    facts: FooterFacts,
) -> Result<LevelWriter, ConvertError> {
    // --- Writer setup (identical to the in-memory path). ---------------------
    // Writer schemas: base + point_count when clustering (Q4) + coalesced_count
    // when coalescing (Q3).
    let geom_name = geom_field.name().clone();
    let (source_schema, cluster_schema, out_schema) =
        build_level_schemas(input_schema, geom_idx, &geom_name, crs, options);

    let writer_levels: Vec<LevelSpec> = emitted
        .iter()
        .map(|e| LevelSpec::new(e.gsd, e.zoom))
        .collect();
    let emitted_gsds: Vec<f64> = emitted.iter().map(|e| e.gsd).collect();
    let level_row_counts: Vec<usize> = emitted.iter().map(|e| e.hint).collect();
    let mut writer_opts = build_writer_options(
        writer_levels,
        &emitted_gsds,
        &level_row_counts,
        crs,
        ranking_provenance,
        renames,
        options,
    )?;
    record_coalesce_merged(writer_opts.generalization.as_mut(), facts.coalesce_merged);
    if let Some(g) = writer_opts.generalization.as_mut() {
        g.zoom_ceiling = facts.zoom_ceiling;
    }
    // #507: `build_writer_options` may have raised the cap to fit parquet's
    // row-group ceiling. Record it before the options move into the writer.
    let effective_max_row_group_size = (writer_opts.max_row_group_size
        != options.max_row_group_size)
        .then_some(writer_opts.max_row_group_size);

    let writer = OverviewWriter::create(output_path, &out_schema, writer_opts)?;

    let non_geom_cols: Vec<usize> = (0..input_schema.fields().len())
        .filter(|&c| c != geom_idx)
        .collect();

    Ok(LevelWriter {
        writer,
        source_schema,
        cluster_schema,
        out_schema,
        non_geom_cols,
        effective_max_row_group_size,
    })
}

/// The winner tables: which level each row belongs to, and how many rows each
/// level gets.
///
/// Built from the pass-1 feature scratch, which this stage frees before it
/// returns. Everything here is O(dataset); pass 2 only carries the row-indexed
/// `min_levels` byte table plus the cluster and coalesce tables.
pub(super) struct WinnerTables {
    /// The resolved level plan: `(gsd, zoom)` per planned level.
    pub(super) level_specs: Vec<(f64, Option<u8>)>,
    /// Cluster tables (Q4), or `None` when clustering is off.
    pub(super) cluster_tables: Option<ClusterTables>,
    /// Per-row geometry kinds (Q3), or `None` when line coalescing is off.
    pub(super) kinds: Option<Vec<FeatureKind>>,
    /// The pass-1 line scratch, kept only when coalescing survives the memory
    /// guard.
    pub(super) coalesce_scratch: Option<CoalesceScratch>,
    /// Coarsest level per INPUT ROW; [`UNASSIGNED_LEVEL`] for skipped rows.
    pub(super) min_levels: Vec<u8>,
    /// Per-level winner counts, cumulative in duplicating mode.
    pub(super) counts: Vec<usize>,
    /// Per planned level, the sorted row indices of the tiny-polygon
    /// accumulator's carriers (#384); empty per level unless it applies.
    pub(super) carriers: Vec<Vec<usize>>,
    pub(super) finest: usize,
}

#[allow(clippy::too_many_arguments)]
fn resolve_winner_tables(
    features: &mut Vec<AssignFeature>,
    acc_values: Vec<Vec<Option<f64>>>,
    areas: Vec<f32>,
    coalesce_scratch: Option<CoalesceScratch>,
    num_rows: usize,
    crs: Crs,
    options: &ConvertOptions,
    peak_rss_mib: &mut Option<f64>,
) -> Result<WinnerTables, ConvertError> {
    // --- Winner tables (assignment + Q2 density budget). ---------------------
    let level_specs = options.levels.resolve(options.gsd_base)?;
    let level_gsds: Vec<f64> = level_specs.iter().map(|(g, _)| *g).collect();

    let t_assign = Instant::now();
    // #306: cap the transient winner-grid memory (the pass-1 peak #300's [rss]
    // logs pinned) at the profile-derived RAM budget; `speed` stays unbounded.
    // Zoom-band representation selector (#317 / #279): per-level
    // representations, parallel to the plan.
    let level_reprs = super::convert::level_representations(&level_specs, &options.representation);
    let assignment = assign_levels_bounded(
        features,
        &level_gsds,
        &options.assign,
        crs,
        super::pipeline::pass1_grid_budget_bytes(options.profile),
        &level_reprs,
    );
    let assign_secs = t_assign.elapsed().as_secs_f64();
    let t_budget = Instant::now();
    let assignment = if options.density.enabled {
        apply_density_budget(
            &assignment,
            features,
            &level_gsds,
            &options.assign,
            &options.density,
            crs,
        )
    } else {
        assignment
    };
    log::debug!(
        "[profile] assignment+budget: {:.2}s (assign {:.2}s + budget {:.2}s)",
        t_assign.elapsed().as_secs_f64(),
        assign_secs,
        t_budget.elapsed().as_secs_f64()
    );
    log::info!(
        "[convert] level assignment complete: {} level(s) in {:.1}s",
        level_gsds.len(),
        t_assign.elapsed().as_secs_f64()
    );
    // Pass-1 winner tables (bboxes/kinds/sort-keys for every feature across all
    // levels) are the O(dataset) peak candidate flagged in #295.
    log_phase_rss("assignment+budget (winner tables)", peak_rss_mib);

    // The feature-parallel winner table (coarsest level per FEATURE, in
    // `features` order) feeds the cluster stage and the per-level counts.
    let feat_min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
    drop(assignment);

    // #384: tiny-polygon accumulator — per level, the carriers that stand in
    // for the sub-visible polygons the level dropped. Row-indexed like the
    // winner table; empty per level unless the accumulator applies there.
    let carriers = streaming_carriers(
        options,
        features,
        &feat_min_levels,
        areas,
        &level_gsds,
        &level_reprs,
        crs,
    );

    // Cluster tables (Q4): built from the pass-1 features + final winner
    // table, before the O(N) scratch is freed. Memory afterwards is
    // O(non-singleton clusters), carried into pass 2 alongside `min_levels`.
    // Accumulate values are extracted per ROW; the cluster stage indexes them
    // by feature position, so remap through each feature's row index.
    let cluster_tables: Option<ClusterTables> = if options.cluster {
        let acc_feat: Vec<Vec<Option<f64>>> = acc_values
            .iter()
            .map(|vals| features.iter().map(|f| vals[f.index]).collect())
            .collect();
        Some(super::convert::build_verified_cluster_tables(
            features,
            &feat_min_levels,
            &level_gsds,
            &acc_feat,
            crs,
            options,
        )?)
    } else {
        None
    };
    drop(acc_values);

    // Coalescing (Q3): keep the per-row kinds (1 byte/row — line rows bypass
    // the winner table at coalesced levels; skipped-geometry rows default to
    // Point, which never matches the Line bypass) and the pass-1 line
    // scratch; apply the memory guard.
    let coalesce_on = coalesce_effective(
        options,
        coalesce_scratch.as_ref().map_or(0, |s| s.rows.len()),
    );
    let kinds: Option<Vec<FeatureKind>> = options.coalesce_lines.then(|| {
        let mut k = vec![FeatureKind::Point; num_rows];
        for f in features.iter() {
            k[f.index] = f.kind;
        }
        k
    });
    let coalesce_scratch = coalesce_scratch.filter(|_| coalesce_on);

    let num_levels = level_gsds.len();
    let finest = num_levels.saturating_sub(1);

    // The ROW-indexed winner table pass 2 addresses (`row_offset + i`), one
    // byte per input row. Skipped-geometry rows keep the UNASSIGNED sentinel,
    // which matches no level in either mode (the level plan is capped at
    // [`super::convert::MAX_LEVELS`] levels, so `finest < u8::MAX`).
    let mut min_levels = vec![UNASSIGNED_LEVEL; num_rows];
    for (f, &ml) in features.iter().zip(&feat_min_levels) {
        min_levels[f.index] = ml;
    }

    // Per-level winner counts (exact row counts in partitioning mode; in
    // duplicating mode exact up to simplification drops — used as the writer's
    // row-group sizing hint and for empty-level omission). With coalescing,
    // line rows leave the winner table at non-canonical levels: their count
    // is the level's surviving chain count instead (computed by running the
    // chain stage per level — cheap relative to decode; the tables are
    // rebuilt, with simplification, per level in pass 2 rather than held for
    // every level at once).
    let mut hist = vec![0usize; num_levels];
    for (f, &ml) in features.iter().zip(&feat_min_levels) {
        if coalesce_scratch.is_some() && f.kind == FeatureKind::Line {
            continue; // counted via the per-level chain stage below
        }
        hist[(ml as usize).min(finest)] += 1;
    }
    drop(feat_min_levels);
    features.clear();
    features.shrink_to_fit(); // free the pass-1 O(N)·48B scratch before pass 2
    let mut counts: Vec<usize> = match options.mode {
        Mode::Duplicating => hist
            .iter()
            .scan(0usize, |acc, &c| {
                *acc += c;
                Some(*acc)
            })
            .collect(),
        Mode::Partitioning => hist,
    };
    // #384: carriers are members of their level only (not of finer ones —
    // there the feature is either a real member already or absent).
    for (count, level_carriers) in counts.iter_mut().zip(&carriers) {
        *count += level_carriers.len();
    }
    if let Some(scratch) = &coalesce_scratch {
        // Duplicating only (partitioning + coalescing is rejected upstream).
        let inputs = scratch.inputs();
        #[allow(clippy::needless_range_loop)]
        for level in 0..num_levels {
            if level == finest {
                counts[level] += scratch.rows.len(); // canonical: verbatim
            } else {
                counts[level] +=
                    coalesce_level_chains(&inputs, level, finest, level_gsds[level], crs, options)
                        .len();
            }
        }
    }

    Ok(WinnerTables {
        level_specs,
        cluster_tables,
        kinds,
        coalesce_scratch,
        min_levels,
        counts,
        carriers,
        finest,
    })
}

/// Footer-only preparation: everything the convert driver settles before the
/// first data page is read.
///
/// Schema and CRS checks, reserved-column renames (#288), attribute-filter
/// binding (#315), row-group pruning (#102 / #315), and the pass-0 staging of
/// the selected groups to local disk (#286/#287).
struct Preflight {
    /// `options` with reserved-column renames applied; the driver borrows this
    /// for the rest of the conversion.
    options: ConvertOptions,
    input_schema: SchemaRef,
    crs: Crs,
    renames: Vec<(String, String)>,
    geom_idx: usize,
    geom_field: Field,
    /// Schema indices of the accumulate columns (Q4).
    acc_cols: Vec<usize>,
    bbox_units: Option<[f64; 4]>,
    bound_filter: Option<super::filter::BoundFilter>,
    selected_row_groups: Option<RowGroupSelection>,
    row_groups_total: usize,
    row_groups_read: usize,
}

fn convert_preflight(
    source: &ConvertSource,
    options: &ConvertOptions,
) -> Result<Preflight, ConvertError> {
    // The UNCACHED probe: `pipeline::available_memory_bytes` must not be
    // touched before pass 1 — its first call freezes a headroom figure every
    // later `auto` decision reuses (#485).
    convert_preflight_with_memory_limit(
        source,
        options,
        super::pipeline::probe_preflight_memory_limit(),
        super::convert::skip_memory_preflight_from_env(),
    )
}

/// [`convert_preflight`], parameterized on the pass-1 memory-floor limit and
/// the escape-hatch decision (#543 test seam: mirrors
/// [`build_writer_options_with_ceiling`]'s #509 pattern). Production always
/// calls it via `convert_preflight` with the real uncached probe; tests pass
/// a tiny mocked limit to prove the #543 hard error fires from
/// footer-derived row counts alone — before `stage_input_pass0` or pass 1
/// ever runs — without needing an actually memory-starved box.
fn convert_preflight_with_memory_limit(
    source: &ConvertSource,
    options: &ConvertOptions,
    memory_limit: Option<super::pipeline::MemoryLimit>,
    skip_memory_preflight: bool,
) -> Result<Preflight, ConvertError> {
    // Schema checks (level column, geometry column) — footer-only reads.
    // (For a remote source, #210, the footer is range-fetched once here and
    // cached across the passes below. For a multi-partition source the
    // schema is the validated union schema and the key-value metadata is
    // partition 0's — construction proved all parts agree.)
    let input_schema: SchemaRef = source.schema()?;

    // CRS detection + rejection (spec Q3) — footer metadata only.
    let kv = source.key_value_metadata()?;
    let crs = super::convert::detect_crs_from_kv(kv.as_ref())?;

    // Reserved-column collisions (#288) are resolved BEFORE row-group
    // selection so the attribute filter (#315) can bind against the final
    // (possibly renamed) schema; the rename is metadata-only and never
    // affects the footer statistics either pruning path reads. See the
    // full #288 rationale on the block below.
    let mut resolved = options.clone();
    let (input_schema, renames) = resolve_reserved_column_collisions(&input_schema, &mut resolved);
    let options = &resolved;

    // Attribute filter (#315): parse + bind against the input schema.
    // Syntax was already validated in `validate_options`; binding resolves
    // column names to indices and type-checks literals.
    let bound_filter = super::convert::bind_attribute_filter(options, &input_schema, &renames)?;

    // Regional extract (#102) + attribute filter (#315): prune input row
    // groups by footer statistics — bbox covering stats for `--bbox`,
    // per-column min/max/null-count stats for `--filter` — before any data
    // pages are read. The two prunings compose by intersection. The
    // selection is PER PART and every pass reads the same selection in the
    // same part order, so the global row indices addressing the winner
    // tables stay aligned. Groups without stats are kept; the exact
    // per-feature filters in pass 1 guarantee identical output either way.
    let row_groups_total = source.num_row_groups_total()?;
    let bbox_units = options
        .bbox
        .map(|b| super::convert::bbox_to_crs_units(&b, crs));
    let selected_row_groups = select_row_groups_streaming(
        source,
        bbox_units.as_ref(),
        bound_filter.as_ref(),
        options.shard.as_ref(),
        crs,
    )?;
    let row_groups_read = selected_row_groups
        .as_ref()
        .map_or(row_groups_total, RowGroupSelection::total_selected);
    // Gated on the two prunings this line NAMES, not on the selection being
    // present: a `--shard` run prunes too, and reporting its selection as an
    // "attribute filter" would send whoever read the log looking for a
    // `--filter` that was never passed. The shard has its own line below.
    if options.bbox.is_some() || bound_filter.is_some() {
        let what = super::convert::pruning_label(options.bbox.is_some(), bound_filter.is_some());
        log::info!("{what} filter: reading {row_groups_read}/{row_groups_total} input row groups");
    }
    if let Some(range) = &options.shard {
        log::info!(
            "[convert] shard {range}: reading {row_groups_read}/{row_groups_total} input row \
             groups (the groups whose bbox reaches this shard's tile range)"
        );
    }
    // #543: preflight the pass-1 feature table's memory floor from footer row
    // counts alone — no I/O beyond the footers already read above — BEFORE
    // pass 1 (or `stage_input_pass0` below) does any real work. Skipped for a
    // `--plan` replay: that path never builds the `Vec<AssignFeature>` table
    // at all, only re-addresses the saved 1-byte/row winner table, so the
    // memory floor this checks does not apply to it. With a per-feature
    // `--bbox`/`--filter` the pruned count is only an upper bound (rows that
    // fail either never become an `AssignFeature`), so it can only warn.
    if options.plan.is_none() {
        let selected_rows = source.selected_row_count(selected_row_groups.as_ref())?;
        super::convert::preflight_pass1_memory(
            selected_rows.max(0) as u64,
            memory_limit,
            skip_memory_preflight,
            bbox_units.is_some() || bound_filter.is_some(),
        )?;
    }
    // #267: nudge toward --bbox / download-first for a large whole-file remote
    // convert (quiet for local inputs and effective bbox extracts).
    super::convert::warn_full_file_remote(source, row_groups_read, row_groups_total);
    // #272: preflight the spill volume. The disk spill (#219) grows to ≈ the
    // selected input bytes — known exactly here, the first moment after
    // row-group selection (summed per part for a multi source) — so compare
    // it against the free space where the spill will live and warn up front
    // (naming the dir and the shortfall) instead of silently degrading to
    // network re-fetch mid-convert.
    super::convert::warn_spill_space(
        source,
        source.selected_input_bytes(selected_row_groups.as_ref())?,
        options.spill_dir.as_deref(),
    );

    // Pass 0 (#286/#287): stage the selected row groups to local disk so both
    // passes below read from the spill, not the network.
    stage_input_pass0(source, selected_row_groups.as_ref(), row_groups_read);

    // (Reserved-column collisions, #288, were resolved above, before the
    // row-group selection: any input column named `level` / `point_count` /
    // `coalesced_count` (case-insensitive) is renamed instead of rejecting
    // the file, keeping the reserved output columns authoritative. The
    // rename preserves column order, so the projection indices pass 1
    // computes against `input_schema` stay valid against the raw file, and
    // pass 2 relabels non-geometry columns positionally into the renamed
    // source schema (`build_source_schema`). `options` was cloned so
    // by-name ranking/accumulate options could be rewritten to the renamed
    // columns.)

    let geom_idx = find_geometry_column(&input_schema).ok_or(ConvertError::NoGeometryColumn)?;
    let geom_field = input_schema.field(geom_idx).clone();

    // Clustering schema checks + accumulate column resolution (Q4).
    let acc_cols = validate_cluster_schema(&input_schema, options)?;
    // Coalescing schema check (Q3).
    validate_coalesce_schema(&input_schema, options)?;

    Ok(Preflight {
        options: resolved.clone(),
        input_schema,
        crs,
        renames,
        geom_idx,
        geom_field,
        acc_cols,
        bbox_units,
        bound_filter,
        selected_row_groups,
        row_groups_total,
        row_groups_read,
    })
}

/// The preflight-derived inputs pass 1 reads. Grouped so
/// [`resolve_plan_state`] can take them as one argument.
struct Pass1Inputs<'a> {
    source: &'a ConvertSource,
    input_schema: &'a Schema,
    geom_idx: usize,
    acc_cols: &'a [usize],
    selected_row_groups: Option<&'a RowGroupSelection>,
    bbox_units: Option<&'a [f64; 4]>,
    bound_filter: Option<&'a super::filter::BoundFilter>,
    crs: Crs,
}

/// Everything the rest of the driver consumes from pass 1 and the level
/// assignment — whether they just ran, or a `--plan` artifact replaced them.
struct PlanState {
    tables: WinnerTables,
    /// Resolved ranking provenance (§3.5); the writer stamps it into the
    /// footer.
    ranking_provenance: RankingProvenance,
    /// Total input rows, INCLUDING skipped-geometry rows.
    num_rows: usize,
    /// Features that survived the scan.
    num_features: usize,
    /// Encoded-geometry bytes across the scan (#305): sizes the pass-2
    /// RAM-vs-spill decision.
    geom_bytes: u64,
    /// Bbox-derived tallies for the report (#188 / #429).
    tallies: BboxTallies,
    pass1_stage_secs: Pass1StageSecs,
    /// Wall time of the pass-1 SCAN alone — from the first read to the last
    /// feature folded in, stopping before the level assignment (#533). A
    /// `Duration`, not an `Instant`: the dump used to carry the start instant
    /// to the end of the run and elapse it there, so `phase_walls.pass1`
    /// silently swallowed the assignment, pass 2 and `writer.finish()`.
    /// Near-zero for a loaded `--plan`, which replaces both stages.
    pass1_wall: Duration,
    /// Wall time of the level assignment (`resolve_winner_tables`): winner
    /// resolution, the density budget, carriers and cluster tables. Zero on
    /// the `--plan` path, where the artifact stands in for it.
    assign_wall: Duration,
}

/// Run pass 1 + the level assignment, or load the artifact that stands in for
/// both (`--plan`), saving one on the way out when `--save-plan` asks.
fn resolve_plan_state(
    inputs: &Pass1Inputs<'_>,
    options: &ConvertOptions,
    peak_rss_mib: &mut Option<f64>,
) -> Result<PlanState, ConvertError> {
    // Captured before either branch: the load path compares the saved
    // fingerprint against it, the save path stores it.
    let flag = if options.plan.is_some() {
        "plan"
    } else {
        "save-plan"
    };
    let fingerprint = (options.save_plan.is_some() || options.plan.is_some())
        .then(|| Fingerprint::capture(inputs.source, inputs.selected_row_groups, options, flag))
        .transpose()?;
    match &options.plan {
        Some(path) => load_plan_state(
            path,
            fingerprint.expect("captured for --plan"),
            options,
            inputs.source,
            inputs.selected_row_groups,
        ),
        None => run_pass1_and_assign(inputs, options, fingerprint, peak_rss_mib),
    }
}

/// The ordinary path: stream the input, assign levels, and (when asked)
/// persist the result before pass 2 starts.
fn run_pass1_and_assign(
    inputs: &Pass1Inputs<'_>,
    options: &ConvertOptions,
    fingerprint: Option<Fingerprint>,
    peak_rss_mib: &mut Option<f64>,
) -> Result<PlanState, ConvertError> {
    let t_pass1 = Instant::now();
    let Pass1Output {
        mut features,
        areas,
        provenance: ranking_provenance,
        acc_values,
        coalesce: coalesce_scratch,
        num_rows,
        skipped_rows,
        geom_bytes,
        pass1_stage_secs,
    } = run_pass1(
        inputs.source,
        inputs.input_schema,
        inputs.geom_idx,
        options,
        inputs.acc_cols,
        inputs.selected_row_groups,
        inputs.bbox_units,
        inputs.bound_filter,
    )?;
    if skipped_rows > 0 {
        log::warn!(
            "skipping {skipped_rows} of {num_rows} input rows with a null, \
             empty, or non-finite geometry"
        );
    }
    let num_features = features.len();
    // Per-kind counts ride along in the plan artifact (a sharded build reads
    // them to size its work); one extra O(N) pass, taken only when saving.
    let kind_counts = options.save_plan.is_some().then(|| count_kinds(&features));

    // One pass over the pass-1 bboxes for every bbox-derived tally: #188
    // antimeridian suspects, and the #429 losses (outside the CRS range, or
    // outside the Web Mercator tiling domain). Warns once per kind and
    // refuses to "succeed" into an empty archive when ~everything is lost.
    let tallies = super::convert::tally_feature_bboxes(&features, inputs.crs)?;

    // Stage markers (#242): everything between pass 1 and the writer used to
    // run in total info-level silence — on planet-scale inputs that was tens
    // of minutes with no output.
    log::info!("[convert] scan complete: {num_features} feature(s) from {num_rows} row(s)");
    // The pass-1 wall STOPS here, at the end of the scan (#533) — everything
    // after this point belongs to the `assign` phase or to pass 2.
    let pass1_wall = t_pass1.elapsed();
    log::debug!(
        "[profile] pass1 stream+scan: {:.2}s",
        pass1_wall.as_secs_f64()
    );
    log_phase_rss("pass1 scan", peak_rss_mib);

    let t_assign = Instant::now();
    let tables = resolve_winner_tables(
        &mut features,
        acc_values,
        areas,
        coalesce_scratch,
        num_rows,
        inputs.crs,
        options,
        peak_rss_mib,
    )?;
    // Stops before `--save-plan` serialization, which is I/O for an opt-in
    // artifact rather than part of the assignment itself.
    let assign_wall = t_assign.elapsed();

    // Persisted here, the first moment the assignment is complete and before
    // pass 2 touches anything: what survives resolve_winner_tables IS the
    // whole dataset-global result (the O(N)·48B feature scratch it folded
    // over is already freed).
    if let Some(path) = &options.save_plan {
        let (n_points, n_lines, n_polygons) = kind_counts.expect("counted when saving a plan");
        let totals = PlanTotals {
            n_rows: num_rows,
            n_features: num_features,
            skipped_rows,
            n_lines,
            n_points,
            n_polygons,
            geom_bytes,
            antimeridian_suspect: tallies.antimeridian_suspect,
            out_of_range: tallies.out_of_range,
            unprojectable: tallies.unprojectable,
        };
        let plan = ConvertPlan::from_winner_tables(
            &tables,
            fingerprint.expect("captured for --save-plan"),
            &ranking_provenance,
            options.entry_zoom.as_ref(),
            totals,
        )?;
        plan.save(path)?;
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        log::info!(
            "[convert] saved the convert plan to {} ({bytes} bytes: {num_rows} row(s), \
             {} level(s)) — re-run with --plan to skip pass 1 and the assignment",
            path.display(),
            tables.level_specs.len(),
        );
    }

    Ok(PlanState {
        tables,
        ranking_provenance,
        num_rows,
        num_features,
        geom_bytes,
        tallies,
        pass1_stage_secs,
        pass1_wall,
        assign_wall,
    })
}

/// `(points, lines, polygons)` over the pass-1 features.
fn count_kinds(features: &[AssignFeature]) -> (usize, usize, usize) {
    let mut counts = (0usize, 0usize, 0usize);
    for f in features {
        match f.kind {
            FeatureKind::Point => counts.0 += 1,
            FeatureKind::Line => counts.1 += 1,
            FeatureKind::Polygon => counts.2 += 1,
        }
    }
    counts
}

/// The `--plan` path: pass 1 and the assignment are replaced wholesale by the
/// saved artifact, after its fingerprint is verified against this run.
fn load_plan_state(
    path: &Path,
    current: Fingerprint,
    options: &ConvertOptions,
    source: &ConvertSource,
    selected_row_groups: Option<&RowGroupSelection>,
) -> Result<PlanState, ConvertError> {
    let t_pass1 = Instant::now();
    let mut plan = ConvertPlan::load(path)?;
    // #498: a data shard narrows the plan's row-group selection and nothing
    // else, so that one fingerprint term is compared as a subset relation
    // instead of an equality. Every other term — the tylertoo version, every
    // thinning option, each part's path, size, mtime, row count and row-group
    // count — stays an exact match.
    let selection_rule = if options.shard.is_some() {
        SelectionRule::SubsetAllowed
    } else {
        SelectionRule::Identical
    };
    plan.fingerprint.verify_with(&current, selection_rule)?;
    if plan.min_levels.len() != plan.totals.n_rows {
        return Err(ConvertError::InvalidConfig(format!(
            "--plan: {} holds {} winner-table row(s) but claims {} input row(s)",
            path.display(),
            plan.min_levels.len(),
            plan.totals.n_rows,
        )));
    }
    // #511/#512: the winner table is addressed by row position, so the plan's
    // row domain must equal what THIS run will actually stream. The per-part
    // footer row counts are already compared field by field above; this
    // catches the dataset-level case the fingerprint cannot see on its own (a
    // plan whose tables disagree with the inputs it names).
    //
    // It used to be gated on `Fingerprint::unpruned_total_rows()`, which
    // returns `None` as soon as ANY part is row-group pruned — so under
    // `--bbox` / `--filter` the one structural check standing between a
    // forged (but checksum-valid) plan and an out-of-bounds index in pass 2
    // simply did not run. The gate is gone: sum `num_rows()` over the row
    // groups this run SELECTED, which is exactly pass 1's row domain, pruned
    // or not, and costs nothing (the footers are already parsed).
    //
    // A shard reads FEWER rows than the plan on purpose, so the comparison
    // moves: the plan's domain is first checked against the row groups the
    // PLAN was saved over (inside `rebase_plan_for_shard`), the row-indexed
    // sections are then re-addressed onto this shard's narrower stream, and
    // the equality below is what proves the re-addressing landed — the
    // compacted table must be exactly as long as what the shard will read.
    if options.shard.is_some() {
        if plan.coalesce.as_ref().is_some_and(|c| !c.rows.is_empty()) {
            return Err(ConvertError::InvalidConfig(format!(
                "--shard is not supported with line coalescing: {} carries {} coalesced line \
                 chain(s). A chain is a NEW geometry spanning every row it merged, so no \
                 single input row group's bbox bounds it — the shard that holds the chain's \
                 row would emit tiles outside its own range while the neighbouring shard, \
                 which never reads that row, would emit none, leaving a gap at the seam. \
                 Re-run the whole fleet (including the coarse job, so the plan matches) with \
                 --no-coalesce-lines.",
                path.display(),
                plan.coalesce.as_ref().map_or(0, |c| c.rows.len()),
            )));
        }
        super::plan_state::rebase_plan_for_shard(&mut plan, source, selected_row_groups, path)?;
    }
    // Captured after the shard re-addressing, so the report and the pass-2
    // memory estimate describe what THIS run will read.
    let totals = plan.totals;
    let will_stream = source.selected_row_count(selected_row_groups)?;
    if will_stream != plan.min_levels.len() as i64 {
        return Err(ConvertError::InvalidConfig(format!(
            "--plan: {} does not match this run's input: the plan holds {} winner-table \
             row(s) but the row group(s) this run will read hold {will_stream} row(s). \
             Re-run without --plan (add --save-plan to write a fresh one).",
            path.display(),
            plan.min_levels.len(),
        )));
    }
    // #512: bound the cluster aggregate arity by this run's accumulate specs,
    // rather than trusting the stride the plan's JSON block declares.
    if let Some(tables) = &plan.cluster_tables {
        let want = options.accumulate.len();
        if let Some(got) = tables
            .iter()
            .flat_map(|t| t.values())
            .map(|e| e.aggregates.len())
            .find(|&n| n != want)
        {
            return Err(ConvertError::InvalidConfig(format!(
                "--plan: {} has cluster entries carrying {got} aggregate(s) but this run \
                 declares {want} --accumulate spec(s)",
                path.display(),
            )));
        }
    }
    if plan.counts.len() != plan.level_specs.len() || plan.finest >= plan.level_specs.len() {
        return Err(ConvertError::InvalidConfig(format!(
            "--plan: {} has an inconsistent level plan ({} spec(s), {} count(s), finest {})",
            path.display(),
            plan.level_specs.len(),
            plan.counts.len(),
            plan.finest,
        )));
    }
    if totals.skipped_rows > 0 {
        log::warn!(
            "skipping {} of {} input rows with a null, empty, or non-finite geometry",
            totals.skipped_rows,
            totals.n_rows
        );
    }
    log::info!(
        "[convert] loaded the convert plan from {} — pass 1 and the level assignment are \
         skipped ({} feature(s) from {} row(s), {} level(s), ranking {:?}, {:.0}% points)",
        path.display(),
        totals.n_features,
        totals.n_rows,
        plan.level_specs.len(),
        plan.rank_plan.mode,
        totals.point_ratio() * 100.0,
    );
    let ranking_provenance = plan.rank_provenance.clone();
    Ok(PlanState {
        tables: plan.into_winner_tables()?,
        ranking_provenance,
        num_rows: totals.n_rows,
        num_features: totals.n_features,
        geom_bytes: totals.geom_bytes,
        tallies: BboxTallies {
            antimeridian_suspect: totals.antimeridian_suspect,
            out_of_range: totals.out_of_range,
            unprojectable: totals.unprojectable,
            // Only feeds the all-lost diagnosis, which already fired (or did
            // not) on the run that produced the plan.
            max_abs_out_of_range: 0.0,
        },
        pass1_stage_secs: Pass1StageSecs::default(),
        // The load stands in for the scan; the assignment did not run at all.
        pass1_wall: t_pass1.elapsed(),
        assign_wall: Duration::ZERO,
    })
}

pub(crate) fn convert_streaming_strategy(
    source: &ConvertSource,
    output_path: &Path,
    options: &ConvertOptions,
    strategy: Pass2Strategy,
) -> Result<ConvertReport, ConvertError> {
    let start = Instant::now();
    // #295: peak-RSS-by-phase instrumentation. Each phase boundary logs process
    // RSS; the max is reported at the end so a single run shows both the peak
    // and which phase produced it.
    let mut peak_rss_mib: Option<f64> = None;

    if options.sort_key.is_some() && options.class_ranking.is_some() {
        return Err(ConvertError::RankingConflict);
    }

    let Preflight {
        options: resolved_options,
        input_schema,
        crs,
        renames,
        geom_idx,
        geom_field,
        acc_cols,
        bbox_units,
        bound_filter,
        selected_row_groups,
        row_groups_total,
        row_groups_read,
    } = convert_preflight(source, options)?;
    let options = &resolved_options;

    // --- Pass 1 + assignment, or the saved plan that replaces them. ----------
    let PlanState {
        tables:
            WinnerTables {
                level_specs,
                mut cluster_tables,
                kinds,
                coalesce_scratch,
                min_levels,
                counts,
                mut carriers,
                finest,
            },
        ranking_provenance,
        num_rows,
        num_features,
        geom_bytes,
        tallies,
        pass1_stage_secs,
        pass1_wall,
        assign_wall,
    } = resolve_plan_state(
        &Pass1Inputs {
            source,
            input_schema: &input_schema,
            geom_idx,
            acc_cols: &acc_cols,
            selected_row_groups: selected_row_groups.as_ref(),
            bbox_units: bbox_units.as_ref(),
            bound_filter: bound_filter.as_ref(),
            crs,
        },
        options,
        &mut peak_rss_mib,
    )?;

    // Planned levels with no winners are omitted (§7.3, #211 auto-clamp);
    // record them for the report + warning.
    let (planned, mut skipped) = partition_emitted_levels(&level_specs, &counts);
    if planned.is_empty() {
        return Err(ConvertError::NoData);
    }
    warn_plan_skipped_levels(&skipped, num_features, planned[0].gsd, planned[0].zoom);

    // #541: the coarse job of a sharded build materializes only the levels it
    // exports. Everything above — pass 1, the assignment, and the plan
    // `--save-plan` has already written — stayed full-range, so a shard
    // consuming that plan cannot tell the difference. `planned` is kept whole
    // for the cascade chains below (a coarse level's geometry is folded
    // through every finer level's GSD, materialized or not); `emitted` is what
    // gets built and written.
    let (kept, truncated_at) = apply_level_ceiling(
        &planned,
        options.zoom_ceiling,
        &mut skipped,
        &mut cluster_tables,
        &mut carriers,
    )?;
    let emitted = &planned[..kept];

    // Only the levels that get written need a chain table (#541); built
    // before the writer because the footer records whether any chain merged.
    let (coalesce_tables, coalesce_merged) = pass2_coalesce(
        coalesce_scratch.as_ref(),
        &planned,
        kept,
        finest,
        crs,
        options,
    );

    let LevelWriter {
        mut writer,
        source_schema,
        cluster_schema,
        out_schema,
        non_geom_cols,
        effective_max_row_group_size,
    } = create_level_writer(
        output_path,
        &input_schema,
        geom_idx,
        &geom_field,
        emitted,
        crs,
        ranking_provenance,
        &renames,
        options,
        FooterFacts {
            coalesce_merged,
            zoom_ceiling: truncated_at,
        },
    )?;

    // --- Pass 2: single-read pipelined engine + canonical streamed last. -----
    // Pass-1 O(N) scratch has been freed by here; this marks the memory floor
    // the pass-2 output sink builds on (its ceiling is the #294 auto choice).
    log_phase_rss("pre-pass2 (winner tables freed)", &mut peak_rss_mib);
    let t_pass2 = Instant::now();

    let duplicating = matches!(options.mode, Mode::Duplicating);
    // Built over the WHOLE planned ladder: a coarse level's cascade chain is
    // the GSD sequence from the finest planned level down to it, and dropping
    // the unmaterialized steps would change the geometry it folds to.
    let cascade_chains = build_cascade_chains(&planned, finest, duplicating, options);
    let cascade_chains = &cascade_chains[..kept];
    let ctxs = build_level_ctxs(
        emitted,
        options,
        &LevelCtxInputs {
            source_schema: &source_schema,
            cluster_schema: &cluster_schema,
            out_schema: &out_schema,
            non_geom_cols: &non_geom_cols,
            geom_idx,
            min_levels: &min_levels,
            acc_cols: &acc_cols,
            kinds: kinds.as_deref(),
            cluster_tables: cluster_tables.as_ref(),
            coalesce_tables: &coalesce_tables,
            cascade_chains,
            carriers: &carriers,
            crs,
            finest,
            duplicating,
        },
    );

    let hints: Vec<usize> = emitted.iter().map(|e| e.hint).collect();

    // Snapshot for the end-of-pass-2 summary: the counter is process-wide,
    // so report the delta from this conversion only (#242).
    let validation_skips_before = validation_skip_count();

    // `(outcome, rows, vertices)` per emitted level, in level order. The
    // outcome distinguishes a written level from one the writer skipped because
    // every candidate collapsed during simplification (#211).
    // Resolve the in-flight depth once (auto-sizes from available cores when
    // the caller left it at IN_FLIGHT_BATCHES_AUTO) and surface it (#264).
    let in_flight_batches = resolve_and_log_in_flight_batches("pass 2", options.in_flight_batches);

    let (level_stats, pass2_engine_timers) = run_pass2_levels(
        &mut writer,
        &ctxs,
        &hints,
        source,
        options,
        selected_row_groups.as_ref(),
        in_flight_batches,
        &out_schema,
        num_rows,
        geom_bytes,
        strategy,
    )?;
    log_validation_skips(validation_skips_before);

    let (mut level_reports, level_spill_bytes) =
        build_level_reports(emitted, level_stats, &mut skipped);
    skipped.sort_by_key(|s| s.planned_level);
    if level_reports.is_empty() {
        // Every emitted level collapsed at write time: no valid overview file
        // can be produced (`levels` MUST be non-empty, §3.3). Under a
        // truncating ceiling that says nothing about the finer levels, so it
        // is the coarse job's legal empty outcome, not a NoData.
        return Err(truncated_at.map_or(ConvertError::NoData, |ceiling| {
            ConvertError::NothingAtOrBelowCeiling { ceiling }
        }));
    }

    // Pass 2's wall STOPS here (#533): the writer finish that follows is its
    // own phase, and what comes after it (`fill_level_bytes`, the report
    // sums) belongs to neither.
    let pass2_wall = t_pass2.elapsed();
    log::debug!("[profile] pass2 total: {:.2}s", pass2_wall.as_secs_f64());
    log_phase_rss("pass2 (output sink)", &mut peak_rss_mib);

    let t_finish = Instant::now();
    let meta = writer.finish()?;
    let writer_finish_wall = t_finish.elapsed();
    log::debug!(
        "[profile] writer.finish: {:.2}s",
        writer_finish_wall.as_secs_f64()
    );
    log_phase_rss("writer.finish", &mut peak_rss_mib);
    log::info!(
        "[rss] convert peak: {}",
        peak_rss_mib.map_or_else(|| "unknown".to_string(), |v| format!("{v:.0} MiB"))
    );
    fill_level_bytes(output_path, &meta, &mut level_reports)?;

    let total_rows: usize = level_reports.iter().map(|l| l.feature_count).sum();
    let total_vertices: usize = level_reports.iter().map(|l| l.vertex_count).sum();
    let total_compressed_bytes: i64 = level_reports.iter().map(|l| l.compressed_bytes).sum();

    // Measurement base for the perf series (pass-1 parallelization, pass-2
    // throughput, checkpoint work): a machine-readable dump of everything the
    // `[profile]`/`[rss]` logs above report by hand, gated behind an env var.
    // Zero effect on output bytes.
    emit_profile_json(ProfileJsonContext {
        options,
        pass1_wall,
        pass1_rows: num_rows,
        pass1_stage_secs,
        assign_wall,
        pass2_wall,
        pass2_rows: total_rows,
        pass2_engine_timers: &pass2_engine_timers,
        writer_finish_wall,
        start,
        level_reports: &level_reports,
        level_spill_bytes: &level_spill_bytes,
        peak_rss_mib,
        in_flight_batches,
    });

    Ok(ConvertReport {
        mode: options.mode,
        levels: level_reports,
        skipped_empty_levels: skipped,
        input_features: num_features,
        total_rows,
        total_vertices,
        total_compressed_bytes,
        row_groups_total,
        row_groups_read,
        antimeridian_suspect_features: tallies.antimeridian_suspect,
        out_of_range_features: tallies.out_of_range,
        unprojectable_features: tallies.unprojectable,
        duration_secs: start.elapsed().as_secs_f64(),
        remote_fetch: super::convert::log_remote_fetch(source),
        effective_max_row_group_size,
    })
}

/// [`convert_streaming_strategy`]'s locals the `TYLERTOO_PROFILE_JSON` dump
/// needs, grouped into a struct so [`emit_profile_json`] can be a single call
/// there (clippy's function-length ceiling leaves no room for inlining this
/// many field computations).
struct ProfileJsonContext<'a> {
    options: &'a ConvertOptions,
    /// Each phase's own wall window, captured AT the phase boundary (#533).
    /// These used to be the phases' start `Instant`s, elapsed here — which
    /// made every one of them run to the end of the conversion.
    pass1_wall: Duration,
    pass1_rows: usize,
    pass1_stage_secs: Pass1StageSecs,
    assign_wall: Duration,
    pass2_wall: Duration,
    pass2_rows: usize,
    pass2_engine_timers: &'a Pass2Timers,
    writer_finish_wall: Duration,
    /// The one genuine end-of-run instant: `total` is elapsed here.
    start: Instant,
    level_reports: &'a [LevelReport],
    level_spill_bytes: &'a [u64],
    peak_rss_mib: Option<f64>,
    in_flight_batches: usize,
}

/// Turn a [`ProfileJsonContext`] into [`ProfileJsonInputs`] and hand it to
/// [`write_profile_json`]. Every phase wall arrives already captured at its
/// own boundary (#533); only `total` is elapsed here, and it is the only
/// window that legitimately ends now.
fn emit_profile_json(ctx: ProfileJsonContext<'_>) {
    write_profile_json(ProfileJsonInputs {
        options: ctx.options,
        pass1_wall_secs: ctx.pass1_wall.as_secs_f64(),
        pass1_rows: ctx.pass1_rows,
        pass1_stage_secs: ctx.pass1_stage_secs,
        assign_wall_secs: ctx.assign_wall.as_secs_f64(),
        pass2_wall_secs: ctx.pass2_wall.as_secs_f64(),
        pass2_rows: ctx.pass2_rows,
        pass2_stage_secs: ctx.pass2_engine_timers.stage_secs(),
        pass2_cascade_step_counts: ctx.pass2_engine_timers.cascade_step_counts(),
        writer_finish_secs: ctx.writer_finish_wall.as_secs_f64(),
        total_secs: ctx.start.elapsed().as_secs_f64(),
        levels: ctx
            .level_reports
            .iter()
            .zip(ctx.level_spill_bytes.iter())
            .map(|(l, &spill_bytes)| (l.feature_count, spill_bytes))
            .collect(),
        peak_rss_mib: ctx.peak_rss_mib,
        in_flight_batches: ctx.in_flight_batches,
    });
}

/// Inputs to [`write_profile_json`], grouped into a struct since the
/// diagnostics dump otherwise needs an unreasonable number of loose scalars.
struct ProfileJsonInputs<'a> {
    options: &'a ConvertOptions,
    /// Wall seconds of the pass-1 SCAN alone (#533) — the level assignment
    /// that follows it has its own entry.
    pass1_wall_secs: f64,
    /// Total INPUT rows pass 1 streamed (matches [`Pass1Output::num_rows`]).
    pass1_rows: usize,
    /// `pass1.stage_secs` in the dump: CORE-SECONDS summed across the reader
    /// thread and the rayon scan chunks (#460), NOT a wall-clock breakdown of
    /// `phase_walls.pass1` — the stages overlap, so the sum is normally
    /// larger. Same convention as `pass2_stage_secs`.
    pass1_stage_secs: Pass1StageSecs,
    /// Wall seconds of the level assignment (`resolve_winner_tables`): winner
    /// resolution, the density budget, carriers and cluster tables. On a
    /// planet-scale run this is one of the largest phases, and before #533 it
    /// had no entry at all — it was hidden inside `pass1`.
    assign_wall_secs: f64,
    pass2_wall_secs: f64,
    /// Total OUTPUT rows written across every level (throughput is measured
    /// in output rows, matching the `[profile] pass2 engine` log).
    pass2_rows: usize,
    pass2_stage_secs: Pass2StageSecs,
    /// `(shared, total)` cascade-fold `Keep` steps (#499,
    /// `pass2.identical_steps` in the dump). Only the pipelined engine's
    /// cascade fold ([`process_batch_cascade`]) records these, so `(0, 0)`
    /// means the fold never ran: a Serial-engine run, or a single-level
    /// ladder (where `run_pass2_buffered` is skipped entirely).
    pass2_cascade_step_counts: (u64, u64),
    writer_finish_secs: f64,
    total_secs: f64,
    /// Per WRITTEN level, in writer order (matches `ConvertReport.levels`):
    /// `(rows, spill_bytes_written)`. `spill_bytes_written` is 0 for a level
    /// buffered in RAM or streamed directly (the finest level).
    levels: Vec<(usize, u64)>,
    peak_rss_mib: Option<f64>,
    in_flight_batches: usize,
}

/// Validate `TYLERTOO_PROFILE_JSON` at option-validation time (#517 S2), if
/// set — called once from `overview::convert::validate_options`, alongside
/// the `--plan` / `--save-plan` preflights (#513), so every path preflight in
/// the convert front end runs in one place and in the same shape.
///
/// [`write_profile_json`] only runs once, at the very end of the run — an
/// unwritable path (typo, missing directory, read-only mount, permissions)
/// was previously reported by a single `log::warn` there, easy to miss in a
/// multi-hour batch/benchmark run that otherwise exits 0 with its profiling
/// data silently gone. This probes the same path in the same mode (append)
/// immediately, so an operator sees the problem before spending the run, not
/// after. Deliberately does not fail the conversion — a diagnostics-only knob
/// must never gate production output — but the warning is made hard to miss.
///
/// Probe-and-remove, matching `preflight_save_plan_writable`: an existing
/// target is opened for append exactly as the dump will open it, and a
/// MISSING one is probed through a uniquely named *sibling* that is removed
/// again. An earlier version opened the target itself with `create(true)`,
/// which left a stray zero-byte dump file behind whenever the run then failed
/// (or the pipeline never reached the dump at all) — a preflight must observe
/// the filesystem, not change it.
pub(super) fn preflight_profile_json_path() {
    let Some(path) = profile_json_target() else {
        return;
    };
    // Fix the run id now, near process start, rather than at the first dump.
    profile_run_id();
    let path = Path::new(&path);
    let probed = if path.exists() {
        // Same mode `write_profile_json` uses, minus `create`: nothing on
        // disk changes, and a directory (or a read-only file) still fails here.
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .map(|_| ())
    } else {
        // `Path::parent` yields `Some("")` for a bare file name: that is `.`.
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let probe = parent.join(format!(
            ".tylertoo-profile-json-probe.{}.{:?}",
            std::process::id(),
            std::thread::current().id(),
        ));
        std::fs::File::create(&probe).map(|_| {
            let _ = std::fs::remove_file(&probe);
        })
    };
    if let Err(e) = probed {
        // #535 review: a `tiles` run preflights twice (convert's
        // `validate_options`, then `export_pmtiles`), and the banner must not
        // print twice for the same bad path. Remembered per path, not as a
        // bare once-per-process flag, so a long-lived host (the Python
        // bindings) that points the var somewhere else still gets warned.
        static WARNED: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);
        if let Ok(mut warned) = WARNED.lock() {
            if warned.as_deref() == Some(path) {
                return;
            }
            *warned = Some(path.to_path_buf());
        }
        let path = path.display();
        log::warn!(
            "[profile] ################################################\n\
             [profile] TYLERTOO_PROFILE_JSON={path} is NOT WRITABLE: {e}\n\
             [profile] Profiling data for THIS RUN will be LOST — the \
             conversion will still proceed and complete normally.\n\
             [profile] ################################################"
        );
    }
}

/// Append one JSON object (one line) with this conversion's stage timing and
/// throughput to the file named by `TYLERTOO_PROFILE_JSON`, if set — the
/// measurement base for the perf series gated on these numbers (pass-1
/// parallelization, pass-2 throughput, checkpoint work). An env var, not a
/// CLI flag, so a diagnostics-only knob costs no CLI-doc churn.
///
/// Units: `phase_walls.*` are WALL seconds; both passes' `stage_secs.*` are
/// CORE-seconds summed across threads (#460 made pass 1 match pass 2 here),
/// so a pass's stage sum normally exceeds its `phase_walls` entry.
///
/// Best-effort and silent-safe: profiling instrumentation must never fail a
/// conversion, so an unset/blank env var is a no-op and an open/write error is
/// only logged. Reads exactly one already-computed number per field (no
/// re-derivation), so this has zero effect on conversion output bytes.
fn write_profile_json(inputs: ProfileJsonInputs<'_>) {
    let Some(path) = profile_json_target() else {
        return;
    };
    let rate = |rows: usize, secs: f64| if secs > 0.0 { rows as f64 / secs } else { 0.0 };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let levels_json: Vec<serde_json::Value> = inputs
        .levels
        .iter()
        .map(|&(rows, spill_bytes)| serde_json::json!({"rows": rows, "spill_bytes": spill_bytes}))
        .collect();
    let (cascade_steps_shared, cascade_steps_total) = inputs.pass2_cascade_step_counts;
    let cascade_share_ratio = if cascade_steps_total > 0 {
        cascade_steps_shared as f64 / cascade_steps_total as f64
    } else {
        0.0
    };
    let value = serde_json::json!({
        "timestamp": timestamp,
        // #535 review: pairs this line with the export line of the same
        // process (`tiles`), and tells the two shapes apart without sniffing
        // for `pass1`/`export` keys.
        "run_id": profile_run_id(),
        "phase": "convert",
        // Disjoint wall-clock windows of one conversion, in run order, so
        // `pass1 + assign + pass2 + writer_finish <= total` always holds
        // (#533 — before that fix each window ran to the end of the run and
        // the four over-counted `total` by ~2x). The remainder of `total` is
        // the preflight, the writer setup and the closing report sums.
        "phase_walls": {
            "pass1": inputs.pass1_wall_secs,
            "assign": inputs.assign_wall_secs,
            "pass2": inputs.pass2_wall_secs,
            "writer_finish": inputs.writer_finish_secs,
            "total": inputs.total_secs,
        },
        "pass1": {
            "rows": inputs.pass1_rows,
            "rows_per_sec": rate(inputs.pass1_rows, inputs.pass1_wall_secs),
            "stage_secs": {
                "read": inputs.pass1_stage_secs.read,
                "decode": inputs.pass1_stage_secs.decode,
                "scan": inputs.pass1_stage_secs.scan,
                "keys": inputs.pass1_stage_secs.keys,
                "assemble": inputs.pass1_stage_secs.assemble,
            },
        },
        "pass2": {
            "rows": inputs.pass2_rows,
            "rows_per_sec": rate(inputs.pass2_rows, inputs.pass2_wall_secs),
            "stage_secs": {
                "read": inputs.pass2_stage_secs.read,
                "decode": inputs.pass2_stage_secs.decode,
                "simplify": inputs.pass2_stage_secs.simplify,
                "build": inputs.pass2_stage_secs.build,
                "drain": inputs.pass2_stage_secs.drain,
                "spill_write": inputs.pass2_stage_secs.spill_write,
            },
            // Cascade-fold Arc-sharing (#499, compute side): how many of the
            // fold's `Keep` steps reused the previous step's geometry
            // (`Arc::clone`) instead of retaining a fresh allocation, because
            // simplification removed nothing. `(0, 0)` whenever the fold never
            // ran: a Serial-engine run (the reference engine doesn't share —
            // see `process_level_batch`), or a single-level ladder.
            "identical_steps": {
                "shared": cascade_steps_shared,
                "total": cascade_steps_total,
                "ratio": cascade_share_ratio,
            },
        },
        "levels": levels_json,
        "peak_rss_mib": inputs.peak_rss_mib,
        "threads": rayon::current_num_threads(),
        "in_flight": inputs.in_flight_batches,
        "memory_profile": inputs.options.profile,
    });
    append_profile_line(&path, &value);
}

/// The `TYLERTOO_PROFILE_JSON` target, or `None` when the var is unset or
/// blank (profiling off). Shared by both profile writers so they agree on
/// what "off" means.
fn profile_json_target() -> Option<String> {
    std::env::var("TYLERTOO_PROFILE_JSON")
        .ok()
        .filter(|p| !p.trim().is_empty())
}

/// Append `value` as one JSONL line to `path` — the single write path both
/// profile writers ([`write_profile_json`], [`write_export_profile_json`])
/// share. Best-effort: an open/write error is only logged.
///
/// The line (with its trailing newline) is serialized up front and written
/// with ONE `write_all` on an `O_APPEND` file (#535 review). The previous
/// `writeln!(f, "{value}")` on an unbuffered `File` went through `Display`,
/// which issues many small `write` syscalls per line — so two processes
/// appending to the same file (concurrent sweep jobs, or shards) could
/// interleave fragments of each other's lines. A single append-mode write of
/// a line this size lands contiguously on local filesystems.
fn append_profile_line(path: &str, value: &serde_json::Value) {
    use std::io::Write;
    let mut line = value.to_string();
    line.push('\n');
    match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                log::warn!("[profile] TYLERTOO_PROFILE_JSON write to {path:?} failed: {e}");
            }
        }
        Err(e) => {
            log::warn!("[profile] TYLERTOO_PROFILE_JSON open {path:?} failed: {e}");
        }
    }
}

/// This process's profile run id (#535 review): `"<pid>-<unix nanos>"`,
/// fixed on first use (in practice at the first preflight, i.e. near process
/// start). Written into every `TYLERTOO_PROFILE_JSON` line, so a `tiles`
/// run's convert and export lines share it, and lines from concurrent runs
/// appending to the same file can be told apart even when their timestamps
/// interleave.
pub(super) fn profile_run_id() -> &'static str {
    static RUN_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RUN_ID.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!("{}-{nanos}", std::process::id())
    })
}

/// Add a pre-measured duration to a nanosecond accumulator — the shared
/// mutator behind every profiling timer set in this module ([`Pass1Timers`],
/// [`Pass2Timers`], [`ExportTimers`]).
pub(super) fn add_nanos(cell: &AtomicU64, dur: Duration) {
    cell.fetch_add(dur.as_nanos() as u64, Ordering::Relaxed);
}

/// Read a nanosecond accumulator as seconds (see [`add_nanos`]).
fn nanos_secs(cell: &AtomicU64) -> f64 {
    Duration::from_nanos(cell.load(Ordering::Relaxed)).as_secs_f64()
}

/// Export-phase [profile] stage counters (#535): nanosecond accumulators,
/// same atomics-across-threads shape as [`Pass2Timers`], shared by `&` down to
/// every rayon closure that records time.
///
/// Units: every field is CORE-SECONDS — the sum of per-item (or
/// single-threaded) `Instant` windows across all threads, exactly the
/// `pass2.stage_secs` convention. Where the work is rayon-parallel, the window
/// is taken INSIDE the innermost per-item closure (one geometry for `clip`,
/// one tile for `encode`), never around a whole parallel section, so a
/// stage's number is the CPU time actually spent in it: it routinely exceeds
/// a level's wall time under parallelism, and never double-counts through
/// nested `par_iter`s or work stealing. Work these windows do not cover (the
/// per-tile member sort before encode, channel waits, rayon scheduling) is
/// uncounted, not mis-attributed. See `docs/PROFILING.md`.
#[derive(Default)]
pub(super) struct ExportTimers {
    /// Reading overview rows: opening the Parquet reader (footer / page-index
    /// setup — paid once per band in partitioning mode, once per wave in
    /// duplicating mode) plus every `ParquetRecordBatchReader::next()` call
    /// (read + Arrow decode). Charged in both modes on a producer thread that
    /// reads ahead of, and concurrently with, the decode/clip consumer: each
    /// wave's producer in [`super::export::process_wave`] (duplicating mode,
    /// #535) and the single-read producer in
    /// [`super::export::fill_member_store`] (partitioning mode, #235) — so it
    /// overlaps `decode`/`clip` in wall time. The producer's `tx.send` (which
    /// can block on the consumer) is excluded.
    pub(super) band_read: AtomicU64,
    /// Everything per batch around the clip that is not the clip: the
    /// geoarrow → `geo::Geometry` decode, the EPSG:3857 → 4326 reprojection
    /// (timed per geometry inside its `par_iter`), property-column
    /// extraction, and per-row member materialization + routing into the
    /// partition buckets / `MemberStore` — in
    /// [`super::export::collect_wave_members`] and
    /// [`super::export::fanout_batch_members`]. A `MemberStore` spill flush
    /// triggered while routing is subtracted out and charged to
    /// `spill_write` instead.
    pub(super) decode: AtomicU64,
    /// [`super::export::feature_tile_members`] (the recursive quadtree clip
    /// cascade), timed per geometry inside the `par_iter` that drives it.
    pub(super) clip: AtomicU64,
    /// Per-tile MVT encode (including the oversized-tile valve) + content
    /// hash + gzip, timed per tile inside [`super::export::encode_members`]'s
    /// parallel section. The member sort that precedes it is not counted.
    pub(super) encode: AtomicU64,
    /// Partitioning mode only: serializing + writing `MemberStore` spill
    /// segments (spill backing), during the fill and its final flush.
    pub(super) spill_write: AtomicU64,
    /// Partitioning mode only: `MemberStore::take_wave` — reading and
    /// decoding a wave's spilled segments back (plus the trivial in-RAM
    /// remainder move; ~0 under RAM backing). Single-threaded, per wave.
    pub(super) spill_read: AtomicU64,
    /// `writer.add_tile_precompressed` calls: the serial per-tile write loop
    /// in [`super::export::export_level`], folded in once per wave.
    pub(super) spool_write: AtomicU64,
    /// `writer.checkpoint(...)` calls (throttled salvage snapshots,
    /// #229/#459). Not the final `writer.finalize(...)` — see
    /// `phase_walls.finalize`.
    pub(super) checkpoint: AtomicU64,
}

/// [`ExportTimers`] snapshotted in seconds for [`write_export_profile_json`].
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ExportStageSecs {
    pub(super) band_read: f64,
    pub(super) decode: f64,
    pub(super) clip: f64,
    pub(super) encode: f64,
    pub(super) spill_write: f64,
    pub(super) spill_read: f64,
    pub(super) spool_write: f64,
    pub(super) checkpoint: f64,
}

impl ExportTimers {
    /// Run `f`, charging its duration to `cell`. The per-item form used
    /// inside rayon closures (one geometry, one tile): an `Instant` pair and a
    /// relaxed `fetch_add` is noise next to a clip or an MVT encode.
    pub(super) fn time<T>(cell: &AtomicU64, f: impl FnOnce() -> T) -> T {
        let t = Instant::now();
        let out = f();
        add_nanos(cell, t.elapsed());
        out
    }

    /// Charge the rest of the enclosing scope to `cell` (drop-guard form of
    /// [`Self::time`], for spans with `?` early returns inside them).
    pub(super) fn scope(cell: &AtomicU64) -> StageScope<'_> {
        StageScope {
            cell,
            start: Instant::now(),
        }
    }

    pub(super) fn stage_secs(&self) -> ExportStageSecs {
        ExportStageSecs {
            band_read: nanos_secs(&self.band_read),
            decode: nanos_secs(&self.decode),
            clip: nanos_secs(&self.clip),
            encode: nanos_secs(&self.encode),
            spill_write: nanos_secs(&self.spill_write),
            spill_read: nanos_secs(&self.spill_read),
            spool_write: nanos_secs(&self.spool_write),
            checkpoint: nanos_secs(&self.checkpoint),
        }
    }
}

/// Drop guard from [`ExportTimers::scope`]: adds the time since it was
/// created to its cell when it goes out of scope.
pub(super) struct StageScope<'a> {
    cell: &'a AtomicU64,
    start: Instant,
}

impl Drop for StageScope<'_> {
    fn drop(&mut self) {
        add_nanos(self.cell, self.start.elapsed());
    }
}

/// Disjoint WALL-clock windows of one export, in run order (#535 review) —
/// the export analogue of convert's `phase_walls`, so
/// `scan + fill + levels + finalize <= total`. The remainder of `total` is
/// the open/validation/writer setup before the scan.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ExportPhaseWalls {
    /// `scan_all_levels` + the #498 range restriction + `plan_levels`.
    pub(super) scan: f64,
    /// The partitioning-mode single-read fill (`fill_member_store`, #235).
    /// Exactly 0.0 when there is no fill (duplicating mode, or the legacy
    /// per-wave path): that work then happens per wave inside `levels`.
    pub(super) fill: f64,
    /// The per-level loop: every level's wave loop plus the throttled
    /// checkpoints between levels.
    pub(super) levels: f64,
    /// `writer.finalize(...)`.
    pub(super) finalize: f64,
}

/// One exported zoom's profile stats (#535): the `export.per_zoom` entries in
/// `TYLERTOO_PROFILE_JSON`. Deliberately a separate type from
/// [`super::export::ZoomReport`] (the `--report` JSON's per-zoom type) so this
/// diagnostics-only knob cannot change the `--report` schema. `tiles` and
/// `features` come from the same per-level counters `export_level` builds its
/// `ZoomReport` from.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct ExportZoomProfile {
    pub(super) zoom: u8,
    /// Wall seconds of this zoom's wave loop in `export_level`, from its
    /// start to the last tile handed to the writer. EXCLUDES the
    /// partitioning-mode fill (`phase_walls.fill`, which does the band reads,
    /// decode and clip for every level up front) and the checkpoint after
    /// the level; in duplicating mode it includes the per-wave reads/clip.
    pub(super) wall_secs: f64,
    pub(super) tiles: usize,
    pub(super) features: usize,
    /// Gzip-compressed tile bytes this zoom PRODUCED (`EncodedTile::data`'s
    /// length summed over every tile handed to the writer), counted BEFORE
    /// the writer's content-hash dedup — a duplicate tile (e.g. an ocean
    /// tile) counts here but is stored once, so this can exceed the zoom's
    /// share of the archive. Not the raw (pre-gzip) MVT size.
    pub(super) bytes: u64,
}

/// Inputs to [`write_export_profile_json`] — the export-phase analogue of
/// [`ProfileJsonInputs`] (#535).
pub(super) struct ExportProfileJsonInputs<'a> {
    /// The archive this export wrote.
    pub(super) output: &'a Path,
    pub(super) layer_name: &'a str,
    /// The overview file's materialization mode (`"partitioning"` /
    /// `"duplicating"`), which decides whether `phase_walls.fill` is used.
    pub(super) mode: &'a str,
    pub(super) phase_walls: ExportPhaseWalls,
    pub(super) total_secs: f64,
    pub(super) stage_secs: ExportStageSecs,
    pub(super) per_zoom: &'a [ExportZoomProfile],
    /// Total wave-loop iterations across every exported level (sum of each
    /// level's `partitions.len().div_ceil(partition_wave)`).
    pub(super) waves_total: usize,
    /// The resolved partition-wave CEILING for this export
    /// ([`super::export::resolve_and_log_partition_wave`]'s return, `auto` or
    /// explicit) — a single scalar, not each level's own #311 auto-narrowed
    /// width.
    pub(super) partition_wave_width: usize,
    /// Number of `writer.checkpoint(...)` calls this export made (throttled
    /// salvage snapshots), not counting the final `finalize`.
    pub(super) checkpoints: u64,
    /// Peak of the RSS samples taken at the export's phase boundaries (after
    /// the scan, the fill, each level, and finalize) — a sampled peak, not a
    /// true high-water mark. `None` when the platform can't report RSS.
    pub(super) peak_rss_mib: Option<f64>,
}

/// Append a SECOND JSON line to `TYLERTOO_PROFILE_JSON`, if set, for the
/// export phase — the export-side analogue of [`write_profile_json`] (#535).
/// See `docs/PROFILING.md`'s "Two JSONL lines for one `tiles` run" section
/// for why this is a second line rather than merged into convert's object;
/// the two are paired by their shared `run_id`. Same best-effort contract as
/// [`write_profile_json`]: an unset/blank env var is a no-op, an open/write
/// error is only logged, and this can never fail (or otherwise affect the
/// bytes of) an export. Only a SUCCESSFUL export reaches this.
pub(super) fn write_export_profile_json(inputs: ExportProfileJsonInputs<'_>) {
    let Some(path) = profile_json_target() else {
        return;
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let per_zoom_json: Vec<serde_json::Value> = inputs
        .per_zoom
        .iter()
        .map(|z| {
            serde_json::json!({
                "zoom": z.zoom,
                "wall_secs": z.wall_secs,
                "tiles": z.tiles,
                "features": z.features,
                "bytes": z.bytes,
            })
        })
        .collect();
    let (walls, stage) = (inputs.phase_walls, inputs.stage_secs);
    let value = serde_json::json!({
        "timestamp": timestamp,
        "run_id": profile_run_id(),
        "phase": "export",
        "output": inputs.output.display().to_string(),
        "layer": inputs.layer_name,
        "export": {
            "mode": inputs.mode,
            "phase_walls": {
                "scan": walls.scan,
                "fill": walls.fill,
                "levels": walls.levels,
                "finalize": walls.finalize,
                "total": inputs.total_secs,
            },
            "stage_secs": {
                "band_read": stage.band_read,
                "decode": stage.decode,
                "clip": stage.clip,
                "encode": stage.encode,
                "spill_write": stage.spill_write,
                "spill_read": stage.spill_read,
                "spool_write": stage.spool_write,
                "checkpoint": stage.checkpoint,
            },
            "per_zoom": per_zoom_json,
            "waves_total": inputs.waves_total,
            "partition_wave_width": inputs.partition_wave_width,
            "checkpoints": inputs.checkpoints,
            "peak_rss_mib": inputs.peak_rss_mib,
            "threads": rayon::current_num_threads(),
        },
    });
    append_profile_line(&path, &value);
}

// ============================================================================
// Pass 1: streaming feature scan + ranking resolution
// ============================================================================

/// A candidate Overture road-class column tracked incrementally during pass 1.
struct RoadCandidate {
    idx: usize,
    ranking: ClassRanking,
    /// Distinct known-vocabulary classes seen so far (detection gate).
    found: HashSet<&'static str>,
    /// Per-row class-rank keys, extracted as we stream.
    keys: Vec<Option<f64>>,
    /// Per-row interned class values (coalescing groups, Q3). Populated
    /// only when coalescing is enabled.
    groups: Vec<u32>,
    interner: GroupInterner,
}

/// The ranking tier resolved from the options + schema *before* reading data
/// (Q1). Mirrors `convert::resolve_ranking`'s tier order; the auto tier needs
/// data (vocab overlap, point majority) so its decision lands after pass 1.
enum RankPlan {
    ExplicitSort {
        idx: usize,
        name: String,
    },
    ExplicitClass {
        idx: usize,
        ranking: ClassRanking,
    },
    Auto {
        roads: Vec<RoadCandidate>,
        confidence: Option<(usize, String)>,
    },
    SizeFallback,
}

/// Build the [`RankPlan`] from the schema, validating explicit columns eagerly
/// (same error variants as the in-memory path).
fn build_rank_plan(schema: &Schema, options: &ConvertOptions) -> Result<RankPlan, ConvertError> {
    if let Some(name) = &options.sort_key {
        let idx = schema
            .index_of(name)
            .map_err(|_| ConvertError::SortKeyColumnMissing { name: name.clone() })?;
        return Ok(RankPlan::ExplicitSort {
            idx,
            name: name.clone(),
        });
    }
    if let Some(cr) = &options.class_ranking {
        let idx =
            schema
                .index_of(&cr.column)
                .map_err(|_| ConvertError::ClassRankColumnMissing {
                    name: cr.column.clone(),
                })?;
        let dt = schema.field(idx).data_type();
        if !matches!(dt, DataType::Utf8 | DataType::LargeUtf8) {
            return Err(ConvertError::ClassRankColumnNotString {
                name: cr.column.clone(),
                data_type: format!("{dt:?}"),
            });
        }
        return Ok(RankPlan::ExplicitClass {
            idx,
            ranking: cr.clone(),
        });
    }
    if !options.no_auto_rank {
        // Candidate Overture road-class columns, in schema order (the first
        // one passing the vocab-overlap gate wins, as in the in-memory path).
        let roads: Vec<RoadCandidate> = schema
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                let lname = f.name().to_ascii_lowercase();
                (lname == "road_class" || lname == "class")
                    && matches!(f.data_type(), DataType::Utf8 | DataType::LargeUtf8)
            })
            .map(|(idx, f)| RoadCandidate {
                idx,
                ranking: overture_road_ranking(f.name().clone()),
                found: HashSet::new(),
                keys: Vec::new(),
                groups: Vec::new(),
                interner: GroupInterner::default(),
            })
            .collect();
        // Candidate Overture places confidence column (point-majority gate is
        // decided after pass 1, once kinds are known).
        let confidence = schema
            .fields()
            .iter()
            .enumerate()
            .find(|(_, f)| {
                f.name().eq_ignore_ascii_case("confidence")
                    && matches!(f.data_type(), DataType::Float32 | DataType::Float64)
            })
            .map(|(idx, f)| (idx, f.name().clone()));
        if !roads.is_empty() || confidence.is_some() {
            return Ok(RankPlan::Auto { roads, confidence });
        }
    }
    Ok(RankPlan::SizeFallback)
}

/// Incrementally scan a string column for known road classes, growing `found`
/// until it reaches [`ROAD_VOCAB_MIN_DISTINCT`] (then stops scanning).
fn scan_road_vocab(col: &dyn Array, found: &mut HashSet<&'static str>) {
    use arrow_array::cast::AsArray;

    if found.len() >= ROAD_VOCAB_MIN_DISTINCT {
        return;
    }
    let vocab: HashSet<&'static str> = KNOWN_ROAD_CLASSES.iter().copied().collect();

    macro_rules! scan {
        ($arr:expr) => {{
            let a = $arr;
            for i in 0..a.len() {
                if a.is_null(i) {
                    continue;
                }
                if let Some(&hit) = vocab.get(a.value(i)) {
                    found.insert(hit);
                    if found.len() >= ROAD_VOCAB_MIN_DISTINCT {
                        return;
                    }
                }
            }
        }};
    }
    match col.data_type() {
        DataType::Utf8 => scan!(col.as_string::<i32>()),
        DataType::LargeUtf8 => scan!(col.as_string::<i64>()),
        _ => {}
    }
}

/// Line geometries (+ compatibility groups) collected during pass 1 for the
/// coalescing stage (Q3). This is the streaming pipeline's one deliberate
/// residual `O(lines)` allocation: chaining needs a level's candidate line
/// geometries together, and the candidate set at every non-canonical
/// duplicating level is ALL lines (chains of sub-visibility fragments must
/// be reclaimable, so no winner-table pre-filter applies). Bounded by
/// [`ConvertOptions::coalesce_max_level_rows`]; beyond it coalescing is
/// skipped and this scratch is never built.
pub(super) struct CoalesceScratch {
    /// Source row index per collected line, ascending input order.
    pub(super) rows: Vec<usize>,
    /// The lines' decoded geometries, parallel to `rows`.
    pub(super) geoms: Vec<Geometry<f64>>,
    /// Sort key per line (Q1 ranking), parallel to `rows`; filled after the
    /// ranking tier resolves.
    pub(super) sort_keys: Vec<Option<f64>>,
    /// Interned class group per line, parallel to `rows`; `None` = no class
    /// ranking active (all lines compatible).
    pub(super) groups: Option<Vec<u32>>,
}

impl CoalesceScratch {
    /// The per-level chaining inputs (borrowing the collected geometries).
    fn inputs(&self) -> Vec<CoalesceInput<'_>> {
        (0..self.rows.len())
            .map(|i| CoalesceInput {
                index: self.rows[i],
                geom: &self.geoms[i],
                sort_key: self.sort_keys[i],
                group: self.groups.as_ref().map_or(0, |g| g[i]),
            })
            .collect()
    }
}

/// Result of [`run_pass1`].
struct Pass1Output {
    /// Per-feature assignment inputs (bbox, kind, resolved sort key).
    features: Vec<AssignFeature>,
    /// Per-feature unsigned polygon area in CRS units² (0 for other kinds),
    /// parallel to `features`; empty unless the tiny-polygon accumulator is
    /// on (#384), since it is the one consumer.
    areas: Vec<f32>,
    /// Resolved ranking provenance (§3.5).
    provenance: RankingProvenance,
    /// Per-accumulate-spec source values (Q4), parallel to `acc_cols`.
    acc_values: Vec<Vec<Option<f64>>>,
    /// Line geometries + groups for coalescing (Q3); `None` unless enabled.
    coalesce: Option<CoalesceScratch>,
    /// Total input rows streamed (INCLUDING skipped-geometry rows): the
    /// domain of every row-indexed table pass 2 addresses.
    num_rows: usize,
    /// Rows skipped for a null, empty, or non-finite geometry (H4).
    skipped_rows: usize,
    /// Total in-memory Arrow byte size of the encoded geometry column across
    /// every scanned batch (#305). `geom_bytes / num_rows` is the measured
    /// average encoded-geometry size per input row that sizes the pass-2
    /// RAM-vs-spill decision; near-free to collect (one buffer-size sum per
    /// batch — no re-encode).
    geom_bytes: u64,
    /// Pass-1 stage split in CORE-SECONDS ([profile] / `TYLERTOO_PROFILE_JSON`
    /// instrumentation, measurement base for the pass-1 parallelization
    /// work). Summed across threads, so it can exceed `phase_walls.pass1` —
    /// see [`Pass1StageSecs`].
    pass1_stage_secs: Pass1StageSecs,
}

/// CORE-SECONDS accumulators for pass-1 stages ([profile] logging), stored as
/// nanoseconds — mirrors [`Pass2Timers`]'s atomics pattern (the two engines
/// stay easy to reconcile), and as of #460 the mirroring is literal: `read`
/// is timed on the dedicated reader thread while `decode`/`scan` run
/// concurrently on the consumer's rayon chunks, so these are SUMMED
/// core-seconds across threads, not wall-clock — `read + decode + scan +
/// keys + assemble` can (and on a multi-core box, should) exceed the run's
/// actual wall time, exactly like [`Pass2Timers`]'s "stage sums are
/// core-seconds, overlap wall" already reads.
#[derive(Default)]
struct Pass1Timers {
    /// Parquet read + Arrow decode of the raw batch (`reader.next()`), timed
    /// on the reader thread — overlaps every other stage below.
    read: AtomicU64,
    /// Geometry column decode (`from_arrow_array` + geometry extraction),
    /// summed across the batch's parallel `scan_chunk` rayon tasks.
    decode: AtomicU64,
    /// Per-row feature scan: attribute-filter eval, `scan_feature` (bbox +
    /// kind), the regional-extract bbox test — summed across the batch's
    /// parallel `scan_chunk` rayon tasks.
    scan: AtomicU64,
    /// Ranking-key, accumulate-value, and entry-zoom-ladder column
    /// extraction — serial (order-dependent, `extract_pass1_batch_keys`),
    /// one measurement per batch.
    keys: AtomicU64,
    /// Post-scan assembly: ranking-tier resolution, sort-key stamping,
    /// entry-level stamping, coalesce-scratch assembly — serial, once for
    /// the whole pass (not per batch).
    assemble: AtomicU64,
}

/// Pass-1 stage split, in **core-seconds** (summed across threads, not wall —
/// see [`Pass1Timers`]) — snapshotted for callers outside this module
/// ([profile] logging and the `TYLERTOO_PROFILE_JSON` dump). As of #460 the
/// stages overlap: `read` runs on the reader thread while `decode`/`scan`
/// run on rayon chunks, so their sum can exceed the pass's wall time.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Pass1StageSecs {
    pub(super) read: f64,
    pub(super) decode: f64,
    pub(super) scan: f64,
    pub(super) keys: f64,
    pub(super) assemble: f64,
}

impl Pass1Timers {
    fn add(cell: &AtomicU64, start: Instant) {
        add_nanos(cell, start.elapsed());
    }
    /// Add a pre-measured duration (used by the reader thread for read time,
    /// mirroring [`Pass2Timers::add_dur`]).
    fn add_dur(cell: &AtomicU64, dur: Duration) {
        add_nanos(cell, dur);
    }
    fn secs(cell: &AtomicU64) -> f64 {
        nanos_secs(cell)
    }
    fn stage_secs(&self) -> Pass1StageSecs {
        Pass1StageSecs {
            read: Self::secs(&self.read),
            decode: Self::secs(&self.decode),
            scan: Self::secs(&self.scan),
            keys: Self::secs(&self.keys),
            assemble: Self::secs(&self.assemble),
        }
    }
    /// Emit the [profile] stage breakdown plus an explicit rows/s figure so a
    /// pass-1 run's throughput is directly comparable across corpora and
    /// machines — the measurement base the pass-1 parallelization work is
    /// gated on.
    fn log_pass1_summary(&self, wall: f64, rows: usize) {
        let s = self.stage_secs();
        let rows_per_sec = if wall > 0.0 { rows as f64 / wall } else { 0.0 };
        log::debug!(
            "[profile] pass1 ({rows} rows): wall={wall:.2}s read={:.2}s decode={:.2}s \
             scan={:.2}s keys={:.2}s assemble={:.2}s rows/s={rows_per_sec:.0} \
             (stage sums are core-seconds, overlap wall)",
            s.read,
            s.decode,
            s.scan,
            s.keys,
            s.assemble,
        );
    }
}

/// Pass 1: stream the input (geometry + ranking/accumulate columns only) and
/// produce the per-feature [`AssignFeature`]s (with resolved sort keys), the
/// ranking provenance block (§3.5), and — when clustering with aggregation —
/// the per-spec source values (parallel to `acc_cols`). Memory: `O(in_flight
/// × read_batch)` transient (#460: the reader thread can run up to
/// [`ConvertOptions::in_flight_batches`] batches ahead of the chunked-scan
/// consumer — the same knob pass 2 already used, was `O(read batch)` when
/// pass 1 was single-threaded) + `O(N)` small per-feature records.
/// The sorted, deduplicated column projection pass 1 reads: geometry +
/// ranking candidates + accumulate columns (Q4) + attribute-filter columns
/// (#315).
fn pass1_projection(
    geom_idx: usize,
    plan: &RankPlan,
    acc_cols: &[usize],
    filter: Option<&super::filter::BoundFilter>,
    ladder_col: Option<usize>,
) -> Vec<usize> {
    let mut cols: Vec<usize> = vec![geom_idx];
    // Entry-zoom ladder (#364): pass 1 decides the level, so its column has
    // to be read here even though nothing else in pass 1 looks at it.
    cols.extend(ladder_col);
    if let Some(f) = filter {
        cols.extend(f.columns().iter().copied());
    }
    match plan {
        RankPlan::ExplicitSort { idx, .. } | RankPlan::ExplicitClass { idx, .. } => cols.push(*idx),
        RankPlan::Auto { roads, confidence } => {
            cols.extend(roads.iter().map(|r| r.idx));
            if let Some((idx, _)) = confidence {
                cols.push(*idx);
            }
        }
        RankPlan::SizeFallback => {}
    }
    cols.extend(acc_cols.iter().copied());
    cols.sort_unstable();
    cols.dedup();
    cols
}

/// Stamp each feature's entry level from the ladder column (#364).
///
/// Resolved against the same level plan the buffered pipeline uses, so both
/// engines place a feature identically. A spec that yields no ladder — an
/// unusable column, a GSD-only plan — leaves every `entry_level` as `None`,
/// which is the "no ladder opinion" case the assignment already handles.
///
/// Lifted out of [`run_pass1`] rather than inlined: pass 1 is already at the
/// cognitive-complexity ceiling the workspace lints enforce, and this is a
/// self-contained step with no other reader in that function.
fn apply_entry_levels(
    options: &ConvertOptions,
    ladder_values: &[Option<f64>],
    num_rows: usize,
    features: &mut [AssignFeature],
) -> Result<(), ConvertError> {
    if options.entry_zoom.is_none() {
        return Ok(());
    }
    debug_assert_eq!(ladder_values.len(), num_rows);
    let level_specs = options.levels.resolve(options.gsd_base)?;
    if let Some(entry) = super::convert::resolve_entry_levels(options, ladder_values, &level_specs)?
    {
        for f in features.iter_mut() {
            f.entry_level = entry.get(f.index).copied().flatten();
        }
    }
    Ok(())
}

/// Upper bound on rows per pass-1 scan chunk (#460): each read batch is
/// sliced into chunks of at most this many rows, each scanned (geometry
/// decode, `scan_feature`, bbox/filter gating) in parallel across a rayon
/// `par_iter`. Large enough that per-chunk overhead (a `RecordBatch::slice`,
/// a `from_arrow_array` re-wrap, and a `Vec` allocation per output field)
/// stays negligible next to the per-row work it parallelizes.
///
/// The size actually used is [`adaptive_pass1_chunk_rows`], not this constant
/// — see there for why a fixed 1024 under-fans out.
const PASS1_CHUNK_ROWS: usize = 1024;

/// Floor on rows per pass-1 scan chunk (#460 review, S3-c). Below roughly
/// this many rows the fixed per-chunk overhead (slice + `from_arrow_array` +
/// four `Vec` allocations, measured at ~8x the per-chunk cost of the decode
/// itself on the geometry-union type) starts to eat the parallel win, so a
/// tiny `--read-batch-size` gets fewer, fatter chunks rather than one task
/// per handful of rows. 256 keeps a 512-row batch at 2 chunks (fan-out
/// preserved) while never going below a chunk the decode can amortize.
const MIN_PASS1_CHUNK_ROWS: usize = 256;

/// Rows per pass-1 scan chunk for a given read-batch size: split the batch
/// across the rayon pool rather than into fixed 1024-row pieces.
///
/// A fixed [`PASS1_CHUNK_ROWS`] fans a default 8192-row batch into only 8
/// chunks — fewer than the cores on a typical machine — and silently
/// disables chunking entirely for `--read-batch-size` below 1024, which is
/// exactly what `docs/OVERVIEW_TUNING.md` tells users to do to cut memory.
/// Clamped to [`MIN_PASS1_CHUNK_ROWS`]..=[`PASS1_CHUNK_ROWS`] so neither end
/// degenerates.
///
/// The merge is size-invariant by construction (chunk-local indices, rebased
/// in ascending chunk order), so this only moves the work split, never the
/// output — pinned by the `run_pass1_with_chunk_rows` equivalence tests.
fn adaptive_pass1_chunk_rows(read_batch_size: usize) -> usize {
    let threads = rayon::current_num_threads().max(1);
    (read_batch_size.max(1) / threads).clamp(MIN_PASS1_CHUNK_ROWS, PASS1_CHUNK_ROWS)
}

/// One batch handed from the pass-1 reader thread to the parallel-scan
/// consumer — mirrors [`pipeline::ReadMsg`], pass 2's equivalent.
struct Pass1ReadMsg {
    batch: RecordBatch,
    read_dur: Duration,
}

/// One line geometry collected while scanning a chunk (Q3 coalescing),
/// chunk-local until the consumer rebases it (see [`ChunkScan`]).
struct ChunkLine {
    /// Row offset within the chunk (0-based).
    local_row: usize,
    /// This line's position within [`ChunkScan::features`] — rebased by the
    /// consumer into a position within the pass's full `features` vector,
    /// exactly like the pre-parallel `line_feat_pos` bookkeeping.
    local_feat_pos: usize,
    geom: Geometry<f64>,
}

/// Result of scanning one chunk of a batch ([`scan_chunk`]): every index
/// inside this struct is CHUNK-LOCAL (0-based within the chunk's own row
/// range). The consumer rebases every index by the chunk's global row/feature
/// offset before merging into the pass's accumulators — never handed a global
/// base, so there is no way to rebase twice or drift (see the module's
/// pass-1 parallelization notes on `run_pass1`).
struct ChunkScan {
    /// Features found in this chunk; `AssignFeature::index` is the row's
    /// offset WITHIN THE CHUNK, rebased by the consumer to `chunk_base + i`.
    features: Vec<AssignFeature>,
    /// #384 polygon areas, parallel to `features` (empty unless enabled).
    areas: Vec<f32>,
    /// One entry per chunk row: was it kept (became a feature)? Length
    /// equals the chunk's row count; merged into the batch-wide `kept_row`
    /// the ladder-blanking step reads.
    kept_row: Vec<bool>,
    lines: Vec<ChunkLine>,
    point_count: usize,
    /// Rows skipped for a null, empty, or non-finite geometry (H4) — NOT
    /// including attribute-filtered or bbox-missed rows, matching the
    /// pre-parallel `skipped_rows` semantics exactly.
    skipped_rows: usize,
}

/// Prefix a pass-1 geometry decode failure with the GLOBAL row range of the
/// chunk it came from (#460 review): the underlying `batch_processor` message
/// carries a *chunk-local* index, which on its own points a user at the wrong
/// row of their file. A `GeoParquetRead` payload is unwrapped rather than
/// nested so the sentence is not repeated twice.
///
/// Diagnostics only — see [`scan_chunk`]'s `diag_row_base`.
fn decode_error_at_rows(diag_row_base: usize, chunk_len: usize, e: crate::Error) -> crate::Error {
    let inner = match &e {
        crate::Error::GeoParquetRead(msg) => msg.clone(),
        other => other.to_string(),
    };
    crate::Error::GeoParquetRead(format!(
        "rows {diag_row_base}..{}: {inner}",
        diag_row_base + chunk_len
    ))
}

/// Scan one chunk of a pass-1 batch: decode its geometry slice, then apply
/// the attribute filter / null-or-invalid-geometry / regional-bbox gates and
/// bucket each surviving row into a chunk-local [`AssignFeature`] — the
/// per-row body of the pre-#460 `run_pass1` loop, unchanged in logic, just
/// scoped to `[0, gcol.len())` instead of a whole batch so it can run as one
/// rayon task among several.
///
/// `diag_row_base` is the chunk's global first-row index and is **DIAGNOSTICS
/// ONLY**: it is read exactly once, by [`decode_error_at_rows`], to name the
/// row range in a decode failure. It must never enter index arithmetic —
/// every index this function produces stays chunk-local and is rebased by the
/// consumer ([`merge_pass1_chunks`]), which is what makes double-rebasing
/// structurally impossible (see [`ChunkScan`]).
#[allow(clippy::too_many_arguments)]
fn scan_chunk(
    geom_field: &Field,
    gcol: &dyn Array,
    diag_row_base: usize,
    filter_mask: Option<&[Option<bool>]>,
    bbox_units: Option<&[f64; 4]>,
    collect_lines: bool,
    want_areas: bool,
    timers: &Pass1Timers,
) -> Result<ChunkScan, ConvertError> {
    let chunk_len = gcol.len();
    let t_decode = Instant::now();
    let garr = from_arrow_array(gcol, geom_field).map_err(|e| {
        decode_error_at_rows(
            diag_row_base,
            chunk_len,
            crate::Error::GeoParquetRead(format!("geometry decode: {e}")),
        )
    })?;
    let mut geoms_buf: Vec<Option<Geometry<f64>>> = Vec::with_capacity(chunk_len);
    extract_geometries_opt_from_array(garr.as_ref(), &mut geoms_buf)
        .map_err(|e| decode_error_at_rows(diag_row_base, chunk_len, e))?;
    Pass1Timers::add(&timers.decode, t_decode);

    let t_scan = Instant::now();
    // Upper bounds: every chunk row could become a feature (or a line, or an
    // area entry when the row is a polygon). Slight over-allocation on a
    // mostly-skipped/mostly-non-polygon chunk beats the repeated reallocation
    // an unsized `Vec::new()` would otherwise do as the chunk fills in.
    let mut features: Vec<AssignFeature> = Vec::with_capacity(chunk_len);
    let mut areas: Vec<f32> = Vec::with_capacity(if want_areas { chunk_len } else { 0 });
    let mut kept_row = vec![false; geoms_buf.len()];
    let mut lines: Vec<ChunkLine> = Vec::with_capacity(if collect_lines { chunk_len } else { 0 });
    let mut point_count = 0usize;
    let mut skipped_rows = 0usize;

    for (i, gopt) in geoms_buf.iter().enumerate() {
        // Attribute filter (#315): keep only rows where the predicate is
        // TRUE. The row index still advances (row-keyed tables stay
        // aligned); the slot stays UNASSIGNED so pass 2 drops it too.
        if let Some(mask) = filter_mask {
            if mask[i] != Some(true) {
                continue;
            }
        }
        let Some(g) = gopt.as_ref() else {
            skipped_rows += 1;
            continue;
        };
        // #274: a single geometry walk yields the usable filter, bbox, and
        // kind (was `usable_geometry` + `geometry_bbox` + `feature_kind`,
        // which traversed the coords twice). `None` == unusable (empty or
        // non-finite), identical to the old `usable_geometry` reject.
        let Some((kind, fbbox)) = scan_feature(g) else {
            skipped_rows += 1;
            continue;
        };
        // Regional extract (#102): a feature whose bbox misses the region
        // produces no AssignFeature — its winner-table slot stays at the
        // UNASSIGNED sentinel, so pass 2 drops the row too. The row index
        // still advances (row-keyed tables stay aligned).
        if let Some(bb) = bbox_units {
            if !super::convert::bboxes_intersect(&fbbox, bb) {
                continue;
            }
        }
        if matches!(kind, FeatureKind::Point) {
            point_count += 1;
        }
        if collect_lines && matches!(kind, FeatureKind::Line) {
            lines.push(ChunkLine {
                local_row: i,
                local_feat_pos: features.len(),
                geom: g.clone(),
            });
        }
        kept_row[i] = true;
        if want_areas {
            areas.push(polygon_area_f32(g));
        }
        features.push(AssignFeature {
            index: i, // chunk-local; the consumer rebases to `chunk_base + i`
            bbox: fbbox,
            kind,
            sort_key: None, // filled below once the ranking tier resolves
            entry_level: None,
        });
    }
    Pass1Timers::add(&timers.scan, t_scan);

    Ok(ChunkScan {
        features,
        areas,
        kept_row,
        lines,
        point_count,
        skipped_rows,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_pass1(
    source: &ConvertSource,
    input_schema: &Schema,
    geom_idx: usize,
    options: &ConvertOptions,
    acc_cols: &[usize],
    row_groups: Option<&RowGroupSelection>,
    bbox_units: Option<&[f64; 4]>,
    filter: Option<&super::filter::BoundFilter>,
) -> Result<Pass1Output, ConvertError> {
    run_pass1_with_chunk_rows(
        source,
        input_schema,
        geom_idx,
        options,
        acc_cols,
        row_groups,
        bbox_units,
        filter,
        adaptive_pass1_chunk_rows(options.read_batch_size),
    )
}

/// Per-batch columnar key extraction for pass 1: ranking keys (explicit
/// sort/class, or the auto-detected road-class/confidence candidates),
/// accumulate-column values (Q4), and the entry-zoom ladder column (#364).
/// Order-dependent — [`GroupInterner`] interns class values in first-seen
/// order — so this always runs serially over the FULL (unsliced) batch, never
/// chunked, exactly as the pre-#460 loop did.
///
/// Pulled out of [`run_pass1_with_chunk_rows`] rather than inlined: it is a
/// self-contained step with no other reader of the intermediate state, and
/// inlining it pushes that function over the workspace's line-count lint.
#[allow(clippy::too_many_arguments)]
fn extract_pass1_batch_keys(
    batch: &RecordBatch,
    proj: &dyn Fn(usize) -> usize,
    plan: &mut RankPlan,
    collect_lines: bool,
    ladder_col: Option<usize>,
    kept_row: &[bool],
    explicit_keys: &mut Vec<Option<f64>>,
    confidence_keys: &mut Vec<Option<f64>>,
    explicit_interner: &mut GroupInterner,
    explicit_groups: &mut Vec<u32>,
    acc_cols: &[usize],
    acc_values: &mut [Vec<Option<f64>>],
    ladder_values: &mut Vec<Option<f64>>,
) -> Result<(), ConvertError> {
    match plan {
        RankPlan::ExplicitSort { idx, .. } => {
            explicit_keys.extend(extract_sort_keys(batch.column(proj(*idx)).as_ref()));
        }
        RankPlan::ExplicitClass { idx, ranking } => {
            let col = batch.column(proj(*idx));
            explicit_keys.extend(extract_class_ranks(col.as_ref(), ranking)?);
            if collect_lines {
                explicit_interner.extend(col.as_ref(), explicit_groups);
            }
        }
        RankPlan::Auto { roads, confidence } => {
            for cand in roads.iter_mut() {
                let col = batch.column(proj(cand.idx));
                scan_road_vocab(col.as_ref(), &mut cand.found);
                cand.keys
                    .extend(extract_class_ranks(col.as_ref(), &cand.ranking)?);
                if collect_lines {
                    cand.interner.extend(col.as_ref(), &mut cand.groups);
                }
            }
            if let Some((idx, _)) = confidence {
                confidence_keys.extend(extract_sort_keys(batch.column(proj(*idx)).as_ref()));
            }
        }
        RankPlan::SizeFallback => {}
    }

    // Accumulate columns (Q4): per-spec source values, in row order.
    // `extract_numeric_values`, not `extract_sort_keys` — aggregating is not
    // ranking, so ±inf is a summand and only NaN is skipped (#428). Must
    // match `convert::extract_accumulate_values`, which the buffered engine
    // uses: the two engines are byte-identical by contract.
    for (s, &idx) in acc_cols.iter().enumerate() {
        acc_values[s].extend(extract_numeric_values(batch.column(proj(idx)).as_ref()));
    }

    // Entry-zoom ladder (#364): row-indexed, like the ranking keys above, but
    // blanked for rows this pass rejected (null/unusable geometry, a false
    // `--filter` predicate, a `--bbox` miss). Those rows produce no feature,
    // so letting their values into the ladder would add rungs the buffered
    // engine never sees and shift every weaker feature by `step`.
    if let Some(idx) = ladder_col {
        let keys = extract_sort_keys(batch.column(proj(idx)).as_ref());
        ladder_values.extend(
            keys.into_iter()
                .zip(kept_row)
                .map(|(k, keep)| if *keep { k } else { None }),
        );
    }
    Ok(())
}

/// Merge one batch's chunk-local [`scan_chunk`] results into the pass's
/// running accumulators, IN ASCENDING CHUNK ORDER, rebasing every chunk-local
/// index (`AssignFeature::index`, a line's row, a line's position in
/// `features`) by `base + chunk_start` as it merges. Returns the batch-wide
/// `kept_row` flags (one per batch row) the ladder-blanking step reads.
///
/// Pulled out of [`run_pass1_with_chunk_rows`] rather than inlined: it is a
/// self-contained step (no other reader of `ranges`/`chunk_results` exists)
/// and inlining it pushes that function over the workspace's line-count lint.
#[allow(clippy::too_many_arguments)]
fn merge_pass1_chunks(
    chunk_results: Vec<Result<ChunkScan, ConvertError>>,
    ranges: &[(usize, usize)],
    base: usize,
    features: &mut Vec<AssignFeature>,
    areas: &mut Vec<f32>,
    line_rows: &mut Vec<usize>,
    line_feat_pos: &mut Vec<usize>,
    line_geoms: &mut Vec<Geometry<f64>>,
    point_count: &mut usize,
    skipped_rows: &mut usize,
) -> Result<Vec<bool>, ConvertError> {
    // The ranges tile the batch contiguously from 0, so the last one's end is
    // the batch row count (0 for an empty batch).
    let n = ranges.last().map_or(0, |&(start, len)| start + len);
    let mut kept_row = vec![false; n];
    for (ci, res) in chunk_results.into_iter().enumerate() {
        let chunk = res?;
        let (start, _len) = ranges[ci];
        let chunk_base = base + start;
        kept_row[start..start + chunk.kept_row.len()].copy_from_slice(&chunk.kept_row);
        *point_count += chunk.point_count;
        *skipped_rows += chunk.skipped_rows;
        // Empty unless the #384 accumulator is on, so no `want_areas` gate is
        // needed here — an empty `extend` is a no-op.
        areas.extend(chunk.areas);
        let feat_offset = features.len();
        for line in chunk.lines {
            line_rows.push(chunk_base + line.local_row);
            line_feat_pos.push(feat_offset + line.local_feat_pos);
            line_geoms.push(line.geom);
        }
        for mut f in chunk.features {
            f.index += chunk_base;
            features.push(f);
        }
    }
    Ok(kept_row)
}

/// [`run_pass1`], with the within-batch scan chunk size as an explicit
/// parameter (production always calls it via [`run_pass1`] with
/// [`PASS1_CHUNK_ROWS`]; tests use this directly to compare a heavily-chunked
/// run against `chunk_rows: usize::MAX` — effectively one chunk per batch,
/// the pre-#460 shape — and assert byte-identical [`Pass1Output`]).
///
/// Reader thread ([`super::pipe::scoped_pipe`], depth = pass 2's resolved
/// in-flight-batches setting) → consumer: each batch is sliced into
/// `chunk_rows`-row chunks (`RecordBatch::slice`, zero-copy), scanned in
/// parallel by [`scan_chunk`] (geometry decode + gating), then merged back in
/// ascending chunk order on the consumer thread. The order-dependent pieces —
/// [`GroupInterner`], the columnar ranking/accumulate/ladder column
/// extraction, and the coalesce-scratch line order — stay serial, running
/// once per batch on the full (unsliced) batch exactly as before; only the
/// per-row geometry decode + `scan_feature` + gating work is chunked and
/// parallelized. A skipped row (null/non-finite geometry, a false
/// `--filter`, or a `--bbox` miss) still advances the row index it would
/// under the serial loop — [`ChunkScan`] returns chunk-local indices and the
/// consumer rebases them to `chunk_base + i` before merging, so a feature's
/// global `index` (and everything keyed by it downstream, notably
/// `Priority::beats`'s `stable_hash(index)` tie-break) is identical
/// regardless of `chunk_rows`.
///
/// Memory: `O(in_flight × read_batch)` transient plus `O(N)` small
/// per-feature records. The reader thread can now run up to `in_flight`
/// batches ([`ConvertOptions::in_flight_batches`]) ahead of the chunked-scan
/// consumer, up from the pre-#460 `O(read batch)` — pass 1 shares the same
/// read/compute overlap knob pass 2 already used.
#[allow(clippy::too_many_arguments)]
fn run_pass1_with_chunk_rows(
    source: &ConvertSource,
    input_schema: &Schema,
    geom_idx: usize,
    options: &ConvertOptions,
    acc_cols: &[usize],
    row_groups: Option<&RowGroupSelection>,
    bbox_units: Option<&[f64; 4]>,
    filter: Option<&super::filter::BoundFilter>,
    chunk_rows: usize,
) -> Result<Pass1Output, ConvertError> {
    let chunk_rows = chunk_rows.max(1);
    let mut plan = build_rank_plan(input_schema, options)?;

    // Entry-zoom ladder column (#364), resolved by name against the (already
    // #288-renamed) schema so a `--magnitude-ladder level` on a source that
    // also has a reserved `level` still finds the caller's column.
    let ladder_col = options
        .entry_zoom
        .as_ref()
        .map(|spec| {
            input_schema.index_of(&spec.column).map_err(|_| {
                ConvertError::InvalidConfig(format!(
                    "entry-zoom column {:?} not found in the input schema",
                    spec.column
                ))
            })
        })
        .transpose()?;

    let cols = pass1_projection(geom_idx, &plan, acc_cols, filter, ladder_col);
    // Original schema index → projected batch column index.
    let proj = |orig: usize| cols.binary_search(&orig).expect("projected column");
    let gcol_idx = proj(geom_idx);

    // Regional extract (#102): read only the bbox-selected row groups
    // (identical per-part selection in pass 2, keeping row indices aligned).
    let mut reader = source.open_stream(&ReadPlan {
        batch_size: options.read_batch_size.max(1),
        projection: Some(&cols),
        row_groups,
    })?;

    let t_pass1_fn = Instant::now();
    let pass1_timers = Pass1Timers::default();
    let timers_ref = &pass1_timers;
    let in_flight = resolve_and_log_in_flight_batches("pass 1", options.in_flight_batches);
    log::debug!("[profile] pass1 chunk_rows={chunk_rows}");

    let mut features: Vec<AssignFeature> = Vec::new();
    // #543 review: pre-size the feature table when its final length is known
    // from the footers — i.e. no per-feature `--bbox`/`--filter` can drop
    // rows (only null/unusable geometries, a small overshoot). Growing by
    // `push` instead costs a transient ~2× the table at the last doubling
    // (realloc = alloc + copy + free under mimalloc): 128 GiB at 1.58B rows.
    // `try_reserve_exact` so an absurd size falls back to growth instead of
    // aborting. Capacity only — output is unchanged.
    if bbox_units.is_none() && filter.is_none() {
        if let Ok(rows) = source.selected_row_count(row_groups) {
            let _ = features.try_reserve_exact(usize::try_from(rows.max(0)).unwrap_or(0));
        }
    }
    // #384: polygon areas for the tiny-polygon accumulator, when it is on.
    let want_areas = accumulator_enabled(options);
    let mut areas: Vec<f32> = Vec::new();
    let mut num_rows = 0usize;
    let mut geom_bytes = 0u64;
    let mut skipped_rows = 0usize;
    let mut point_count = 0usize;
    let mut explicit_keys: Vec<Option<f64>> = Vec::new();
    let mut confidence_keys: Vec<Option<f64>> = Vec::new();
    let mut acc_values: Vec<Vec<Option<f64>>> = vec![Vec::new(); acc_cols.len()];
    // Entry-zoom ladder column values (#364), row-indexed.
    let mut ladder_values: Vec<Option<f64>> = Vec::new();
    // Coalescing (Q3): line rows + geometries, and — for an explicit class
    // ranking — the interned per-row class groups. `line_feat_pos` holds each
    // line's position in `features` (NOT its row index: skipped-geometry rows
    // make the two diverge).
    let collect_lines = options.coalesce_lines;
    let mut line_rows: Vec<usize> = Vec::new();
    let mut line_feat_pos: Vec<usize> = Vec::new();
    let mut line_geoms: Vec<Geometry<f64>> = Vec::new();
    let mut explicit_groups: Vec<u32> = Vec::new();
    let mut explicit_interner = GroupInterner::default();

    scoped_pipe(
        in_flight,
        // Producer: read batches in order until EOF or the consumer hangs
        // up. A read error propagates via `?` (real failure, not a
        // disconnect); a `SendError` means the consumer stopped — a clean
        // `break`, per `scoped_pipe`'s contract.
        |tx: &Sender<Pass1ReadMsg>| -> Result<(), ConvertError> {
            loop {
                let t_read = Instant::now();
                let batch = match reader.next() {
                    None => break,
                    Some(res) => res?,
                };
                let read_dur = t_read.elapsed();
                if tx.send(Pass1ReadMsg { batch, read_dur }).is_err() {
                    break;
                }
            }
            Ok(())
        },
        // Consumer: process batches in read order. Within a batch, the
        // geometry decode + per-row scan/gate fans out over `chunk_rows`-row
        // chunks via rayon (order-preserving `collect`); the order-dependent
        // columnar extraction below stays serial on the full batch, exactly
        // as the pre-#460 loop did.
        |rx: Receiver<Pass1ReadMsg>| -> Result<(), ConvertError> {
            for msg in rx.iter() {
                Pass1Timers::add_dur(&timers_ref.read, msg.read_dur);
                let batch = msg.batch;
                let n = batch.num_rows();
                let base = num_rows;
                let schema = batch.schema();
                let gfield = schema.field(gcol_idx).clone();

                let t_filter = Instant::now();
                // Attribute filter (#315): evaluate the predicate over the
                // projected batch once, up front — `eval_expr` is a pure
                // per-row column comparison (no cross-row context), so the
                // mask can be sliced per chunk below.
                let filter_mask: Option<Vec<Option<bool>>> =
                    filter.map(|f| f.eval_mask(&batch, &proj));
                Pass1Timers::add(&timers_ref.scan, t_filter);

                // Chunk boundaries: contiguous, non-overlapping, ascending.
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                let mut off = 0usize;
                while off < n {
                    let len = chunk_rows.min(n - off);
                    ranges.push((off, len));
                    off += len;
                }

                let chunk_results: Vec<Result<ChunkScan, ConvertError>> = ranges
                    .par_iter()
                    .map(|&(start, len)| {
                        // Slice only the geometry column (not the whole
                        // `RecordBatch`, which would touch every non-geometry
                        // column's offsets too, for no benefit here).
                        let gcol = batch.column(gcol_idx).slice(start, len);
                        let mask_slice = filter_mask.as_deref().map(|m| &m[start..start + len]);
                        scan_chunk(
                            &gfield,
                            gcol.as_ref(),
                            // Diagnostics only (see `scan_chunk`): the chunk's
                            // global first row, used solely to name the row
                            // range in a decode error. NOT an index base —
                            // the merge below still rebases every chunk-local
                            // index itself.
                            base + start,
                            mask_slice,
                            bbox_units,
                            collect_lines,
                            want_areas,
                            timers_ref,
                        )
                    })
                    .collect();

                // Merge chunks IN ORDER, rebasing every chunk-local index by
                // this chunk's global row/feature offset — never a global
                // base handed into `scan_chunk` itself (H4: a skipped row
                // must still advance the row index exactly as the serial
                // loop advanced it, so every row-keyed table stays aligned).
                let kept_row = merge_pass1_chunks(
                    chunk_results,
                    &ranges,
                    base,
                    &mut features,
                    &mut areas,
                    &mut line_rows,
                    &mut line_feat_pos,
                    &mut line_geoms,
                    &mut point_count,
                    &mut skipped_rows,
                )?;
                num_rows += n;
                // #305: measure the encoded geometry column's in-memory size
                // so the pass-2 RAM-vs-spill estimate can use this input's
                // actual average geometry weight instead of a
                // one-size-fits-all constant. O(#buffers) per batch — no
                // per-row work, no re-encode, unaffected by chunking.
                geom_bytes += batch.column(gcol_idx).get_array_memory_size() as u64;

                let t_keys = Instant::now();
                extract_pass1_batch_keys(
                    &batch,
                    &proj,
                    &mut plan,
                    collect_lines,
                    ladder_col,
                    &kept_row,
                    &mut explicit_keys,
                    &mut confidence_keys,
                    &mut explicit_interner,
                    &mut explicit_groups,
                    acc_cols,
                    &mut acc_values,
                    &mut ladder_values,
                )?;
                Pass1Timers::add(&timers_ref.keys, t_keys);
            }
            Ok(())
        },
    )?;

    let t_assemble = Instant::now();
    let (keys, provenance, all_groups) = resolve_ranking_tier(
        plan,
        explicit_keys,
        confidence_keys,
        explicit_groups,
        collect_lines,
        features.len(),
        point_count,
    );

    if let Some(keys) = keys {
        // Keys are extracted per ROW (including skipped-geometry rows), so
        // they are looked up by each feature's row index, not zipped
        // positionally.
        debug_assert_eq!(keys.len(), num_rows);
        for f in features.iter_mut() {
            f.sort_key = keys[f.index];
        }
    }

    apply_entry_levels(options, &ladder_values, num_rows, &mut features)?;

    // Coalescing scratch (Q3): line sort keys + per-line groups. `rows` and
    // `groups` are row-indexed; sort keys live on the features.
    let coalesce = collect_lines.then(|| CoalesceScratch {
        sort_keys: line_feat_pos
            .iter()
            .map(|&p| features[p].sort_key)
            .collect(),
        groups: all_groups.map(|g| line_rows.iter().map(|&r| g[r]).collect()),
        rows: line_rows,
        geoms: line_geoms,
    });
    Pass1Timers::add(&pass1_timers.assemble, t_assemble);

    pass1_timers.log_pass1_summary(t_pass1_fn.elapsed().as_secs_f64(), num_rows);

    Ok(Pass1Output {
        features,
        areas,
        provenance,
        acc_values,
        coalesce,
        num_rows,
        skipped_rows,
        geom_bytes,
        pass1_stage_secs: pass1_timers.stage_secs(),
    })
}

// ============================================================================
// Pass 2: per-level streaming filter → simplify → write
// ============================================================================

/// Pass-2 [profile] counters: wall-time accumulators per stage (stored as
/// nanoseconds), plus two cascade-fold step counters (#499) that piggyback on
/// the same shared-atomics lifecycle rather than nanosecond timings. Atomic so
/// the pipelined engine ([`super::pipeline`]) can share one set across the
/// parallel per-level processing of a batch; the serial
/// [`write_level_streaming`] path uses it single-threaded.
#[derive(Default)]
pub(super) struct Pass2Timers {
    /// Parquet read + Arrow decode of the raw batch (`reader.next()`).
    ///
    /// Core-seconds, not wall: with `--read-workers > 1` this accumulates on
    /// the reader worker threads (#494) and can exceed the pass's wall time —
    /// that is the point, since it is what the parallel read overlaps. The
    /// in-order merge's own cost (the `concat_batches` splice at a worker
    /// seam, and the time the merge spends parked on `recv`) is deliberately
    /// NOT counted here: the splice is charged to nobody, and counting the
    /// `recv` wait would double-count time the workers are already reporting.
    read: AtomicU64,
    /// Winner selection + geometry take/decode to `geo::Geometry`.
    decode: AtomicU64,
    /// Simplification (or verbatim vertex counting at the canonical level).
    simplify: AtomicU64,
    /// Output batch assembly (`build_level_batch`).
    build: AtomicU64,
    /// Draining a level's sink into `writer.write_level` (the serial parquet
    /// append). Previously invisible: it runs after the read loop finishes,
    /// one level at a time.
    drain: AtomicU64,
    /// The bounded-profile Arrow IPC spill write: encode + write on the
    /// per-level spill-writer thread (#494), folded in when that thread is
    /// joined (`SpillState::into_reader`), not as batches are pushed. So it is
    /// core-seconds off the consumer's critical path — it no longer runs in
    /// `SpillState::push` and no longer runs on the consumer thread.
    spill_write: AtomicU64,
    /// Cascade-fold steps ([`process_batch_cascade`], #499) that reused
    /// (`Arc::clone`) the previous step's geometry because simplification
    /// removed nothing, instead of retaining a fresh allocation. Wired into
    /// the `TYLERTOO_PROFILE_JSON` dump (`pass2.identical_steps`) so a run
    /// quantifies its own share-vs-clone ratio.
    cascade_steps_shared: AtomicU64,
    /// Total cascade-fold `Keep` steps evaluated ([`process_batch_cascade`]);
    /// the denominator for `cascade_steps_shared`.
    cascade_steps_total: AtomicU64,
}

/// Pass-2 engine stage wall-time split, in seconds — [`Pass2Timers`]
/// snapshotted for callers outside this module ([profile] JSON dump).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Pass2StageSecs {
    pub(super) read: f64,
    pub(super) decode: f64,
    pub(super) simplify: f64,
    pub(super) build: f64,
    pub(super) drain: f64,
    pub(super) spill_write: f64,
}

impl Pass2Timers {
    fn add(cell: &AtomicU64, start: Instant) {
        add_nanos(cell, start.elapsed());
    }
    /// Add a pre-measured duration (used by the reader thread for read time).
    pub(super) fn add_dur(cell: &AtomicU64, dur: Duration) {
        add_nanos(cell, dur);
    }
    fn secs(cell: &AtomicU64) -> f64 {
        nanos_secs(cell)
    }
    pub(super) fn read_cell(&self) -> &AtomicU64 {
        &self.read
    }
    pub(super) fn drain_cell(&self) -> &AtomicU64 {
        &self.drain
    }
    pub(super) fn spill_write_cell(&self) -> &AtomicU64 {
        &self.spill_write
    }
    /// Record a batch's worth of cascade-fold `Keep` steps (#499): `shared`
    /// reused the previous step's `Arc<Geometry>` because simplification
    /// removed nothing; `total` is every `Keep` step evaluated. Called once
    /// per batch from [`process_batch_cascade`] (after summing across that
    /// batch's parallel per-feature folds), not once per step, to keep the
    /// atomics off the hot per-feature path.
    pub(super) fn record_cascade_steps(&self, shared: u64, total: u64) {
        self.cascade_steps_shared
            .fetch_add(shared, Ordering::Relaxed);
        self.cascade_steps_total.fetch_add(total, Ordering::Relaxed);
    }
    /// Snapshot `(shared, total)` cascade-fold step counts (#499,
    /// `TYLERTOO_PROFILE_JSON`'s `pass2.identical_steps`).
    pub(super) fn cascade_step_counts(&self) -> (u64, u64) {
        (
            self.cascade_steps_shared.load(Ordering::Relaxed),
            self.cascade_steps_total.load(Ordering::Relaxed),
        )
    }
    /// Fold this timer set's per-stage totals into `other` (adds, never
    /// overwrites). Used to combine the finest level's own
    /// [`write_level_streaming`] timers into the pipelined engine's
    /// accumulator ([profile] / `TYLERTOO_PROFILE_JSON`, #517 S1) — without
    /// this, `pass2.stage_secs` in the dump omitted the finest — and largest
    /// — level entirely, while `pass2.rows` and `phase_walls.pass2` already
    /// included it.
    pub(super) fn fold_into(&self, other: &Pass2Timers) {
        other
            .read
            .fetch_add(self.read.load(Ordering::Relaxed), Ordering::Relaxed);
        other
            .decode
            .fetch_add(self.decode.load(Ordering::Relaxed), Ordering::Relaxed);
        other
            .simplify
            .fetch_add(self.simplify.load(Ordering::Relaxed), Ordering::Relaxed);
        other
            .build
            .fetch_add(self.build.load(Ordering::Relaxed), Ordering::Relaxed);
        other
            .drain
            .fetch_add(self.drain.load(Ordering::Relaxed), Ordering::Relaxed);
        other
            .spill_write
            .fetch_add(self.spill_write.load(Ordering::Relaxed), Ordering::Relaxed);
        other.cascade_steps_shared.fetch_add(
            self.cascade_steps_shared.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
        other.cascade_steps_total.fetch_add(
            self.cascade_steps_total.load(Ordering::Relaxed),
            Ordering::Relaxed,
        );
    }
    pub(super) fn stage_secs(&self) -> Pass2StageSecs {
        Pass2StageSecs {
            read: Self::secs(&self.read),
            decode: Self::secs(&self.decode),
            simplify: Self::secs(&self.simplify),
            build: Self::secs(&self.build),
            drain: Self::secs(&self.drain),
            spill_write: Self::secs(&self.spill_write),
        }
    }
    /// Emit the aggregated per-stage breakdown ([profile] logging) for the
    /// pipelined engine, where stages interleave across levels so a per-level
    /// split is not meaningful.
    pub(super) fn log_engine_summary(&self, total_secs: f64, rows: usize) {
        let s = self.stage_secs();
        log::debug!(
            "[profile] pass2 engine ({rows} rows): wall={total_secs:.2}s \
             read={:.2}s decode={:.2}s simplify={:.2}s build={:.2}s \
             drain={:.2}s spill_write={:.2}s (stage sums are core-seconds, \
             overlap wall)",
            s.read,
            s.decode,
            s.simplify,
            s.build,
            s.drain,
            s.spill_write,
        );
    }
}

/// Immutable context for one level's pass-2 stream.
pub(super) struct LevelStreamCtx<'a> {
    source_schema: &'a Schema,
    /// `source_schema` + trailing `point_count` when clustering, otherwise
    /// identical (the schema [`apply_cluster_columns`] produces).
    cluster_schema: &'a Schema,
    /// Final writer schema: `cluster_schema` + trailing `coalesced_count`
    /// when coalescing, otherwise identical.
    out_schema: &'a Schema,
    non_geom_cols: &'a [usize],
    geom_idx: usize,
    /// Winner table: per input row, its coarsest level.
    min_levels: &'a [u8],
    /// Level index in the *resolved* plan (membership is tested against this,
    /// not the emitted/renumbered index).
    orig_level: u8,
    duplicating: bool,
    verbatim: bool,
    gsd_m: f64,
    /// Zoom-band representation (#317 / #279): how this level renders
    /// polygonal features (full geometry, representative points, or
    /// dithered placeholder squares for the below-tolerance ones).
    repr: Representation,
    crs: Crs,
    simplify: &'a SimplifyOptions,
    /// Clustering (Q4): append `point_count` + rewrite accumulate columns.
    cluster_enabled: bool,
    /// This level's cluster table; `None` at the canonical level (singletons)
    /// or when clustering is off.
    cluster_table: Option<&'a std::collections::HashMap<usize, ClusterEntry>>,
    /// Schema indices of the accumulate columns.
    acc_cols: &'a [usize],
    /// Coalescing (Q3): append `coalesced_count` at every level.
    coalesce_enabled: bool,
    /// Per-row geometry kinds (line rows bypass the winner table at
    /// coalesced levels); `Some` iff coalescing is enabled.
    kinds: Option<&'a [FeatureKind]>,
    /// This level's chain table (rep row → merged simplified geometry +
    /// member count); `None` at verbatim levels or when coalescing is
    /// off/guard-skipped.
    coalesce_table: Option<&'a CoalesceTable>,
    /// Cascading simplification (#218): fine→coarse step chain ending at
    /// this level (`[step_finest-1, …, step_this]`), fed to
    /// [`simplify_cascade`]. Empty when cascading does not apply (cascade
    /// off, partitioning, or verbatim level) — the level then simplifies
    /// canonical geometry directly with `gsd_m` / `point_repr`.
    cascade_chain: &'a [CascadeStep],
    /// Tiny-polygon accumulator carriers at this level (#384): sorted row
    /// indices of polygons that are NOT members (`min_level > orig_level`)
    /// but are emitted as a placeholder square standing in for the dropped
    /// area around them. Empty unless the accumulator applies.
    carriers: &'a [usize],
}

impl LevelStreamCtx<'_> {
    /// Is row `g` emitted at this level: a winner-table member, or a
    /// tiny-polygon carrier (#384)?
    #[inline]
    fn is_member(&self, g: usize) -> bool {
        let ml = self.min_levels[g];
        if self.duplicating {
            ml <= self.orig_level || is_carrier(self.carriers, g)
        } else {
            ml == self.orig_level
        }
    }

    /// Is row `g` a carrier here (emitted as a square, not as itself)?
    #[inline]
    fn is_carrier_row(&self, g: usize) -> bool {
        self.duplicating && self.min_levels[g] > self.orig_level && is_carrier(self.carriers, g)
    }
}

impl LevelStreamCtx<'_> {
    /// Whether the pipelined engine should process batches through the
    /// cascade fan-out ([`process_batch_cascade`], #218): duplicating mode
    /// with cascading enabled. Uniform across a conversion's level set.
    pub(super) fn is_cascading_duplicating(&self) -> bool {
        self.duplicating && self.simplify.cascade
    }
}

/// Stream one level from the input file into the writer. Returns the writer
/// outcome (a level whose every candidate collapses during simplification is
/// skipped, #211), `(rows_written, vertex_count)`, plus this level's own
/// [`Pass2Timers`] (#517 S1) — the caller folds it into the shared
/// `TYLERTOO_PROFILE_JSON` accumulator, since the finest level streamed here
/// never otherwise reaches `run_pass2_buffered`'s returned timers.
#[allow(clippy::too_many_arguments)]
fn write_level_streaming(
    writer: &mut OverviewWriter<File>,
    level_idx: usize,
    hint: usize,
    source: &ConvertSource,
    read_tuning: ReadTuning,
    in_flight: usize,
    row_groups: Option<&RowGroupSelection>,
    ctx: &LevelStreamCtx<'_>,
) -> Result<(LevelWriteOutcome, usize, usize, Pass2Timers), ConvertError> {
    let rows = Cell::new(0usize);
    let vertices = Cell::new(0usize);
    // Writer-thread time spent blocked waiting on the producer; the writer's
    // own busy time is `total - recv_wait` ([profile] logging).
    let recv_wait_ns = Cell::new(0u64);
    // Owned here (returned to the caller at the end); `timers` below is a `&`
    // to it, shared into the producer closure and used for the per-level
    // debug line — NLL ends that borrow before the move on return.
    let level_timers = Pass2Timers::default();
    let timers = &level_timers;
    let fallbacks_before = full_resolution_fallback_count();
    let t_level = Instant::now();

    // One processed output batch handed from the producer to the writer.
    struct Processed {
        batch: RecordBatch,
        verts: usize,
    }

    // Overlap decode→process with the single-threaded parquet writer (#264,
    // extending the #213 pipeline discipline to the streamed finest level): a
    // producer thread reads input batches and runs `process_level_batch`
    // (read + geometry decode + simplify + assemble), pushing finished output
    // batches over a bounded channel; the writer drains it on this thread.
    // Batches stay in read order (FIFO channel, single producer), so output —
    // and therefore row-group boundaries — are byte-identical to a serial
    // build. Channel depth bounds read/compute run-ahead the same way the
    // buffered engine's reader channel does.
    // `timers` (bound above as `&level_timers`) is shared by reference into
    // the producer thread: a `&Pass2Timers` is `Copy`, so the producer
    // closure copies the borrow and leaves `level_timers` owned here for the
    // post-scope read and the return value below.
    let outcome = scoped_pipe(
        in_flight,
        // Producer: read + process, in order, until EOF or the writer
        // hangs up. Returns the first stream/processing error, if any.
        // Everything the closure touches (`source`, `ctx`, `&timers`,
        // `row_groups`, the `usize`s) is `Copy`, so the outer bindings —
        // notably `timers`, read back afterwards — stay valid.
        |tx: &Sender<Processed>| -> Result<(), ConvertError> {
            // Heartbeat (#242): the finest level re-streams the whole
            // input; keep the operator informed on planet-scale files
            // (quiet on small ones).
            let mut last_progress = Instant::now();
            // Regional extract (#102): read the same per-part bbox-selected
            // row groups as pass 1, so the winner tables' global row indices
            // line up. One reader, or several merged back into the identical
            // batch sequence (#494) — `read_tuning` decides, and the decision
            // never reaches the output.
            read_in_order(
                source,
                row_groups,
                read_tuning,
                |batch, offset, read_dur| {
                    if last_progress.elapsed().as_secs() >= 10 {
                        last_progress = Instant::now();
                        log::info!("[convert] level {level_idx}: {offset} input row(s) scanned");
                    }
                    Pass2Timers::add_dur(&timers.read, read_dur);
                    match process_level_batch(&batch, offset, ctx, timers)? {
                        // No members of this level in the batch.
                        None => Ok(ReadFlow::Continue),
                        Some((out, verts)) => Ok(
                            // Writer gone (it errored and dropped the
                            // receiver): stop; the writer's error is reported
                            // by the caller.
                            if tx.send(Processed { batch: out, verts }).is_err() {
                                ReadFlow::Stop
                            } else {
                                ReadFlow::Continue
                            },
                        ),
                    }
                },
            )
        },
        // Writer (this thread): drain processed batches in order. Dropping
        // the producer's sender (EOF, error, or writer-gone) fuses `recv`;
        // `scoped_pipe` owns the mirror-image guarantee that this receiver is
        // dropped before the producer is joined (#362).
        |rx: Receiver<Processed>| -> Result<LevelWriteOutcome, ConvertError> {
            let batches = std::iter::from_fn(|| {
                let t_wait = Instant::now();
                match rx.recv() {
                    Ok(msg) => {
                        recv_wait_ns.set(recv_wait_ns.get() + t_wait.elapsed().as_nanos() as u64);
                        rows.set(rows.get() + msg.batch.num_rows());
                        vertices.set(vertices.get() + msg.verts);
                        Some(msg.batch)
                    }
                    Err(_) => {
                        recv_wait_ns.set(recv_wait_ns.get() + t_wait.elapsed().as_nanos() as u64);
                        None
                    }
                }
            });
            Ok(writer.write_level(level_idx, Some(hint), batches)?)
        },
    )?;
    let total = t_level.elapsed().as_secs_f64();
    let read_s = Pass2Timers::secs(&timers.read);
    let decode_s = Pass2Timers::secs(&timers.decode);
    let simplify_s = Pass2Timers::secs(&timers.simplify);
    let build_s = Pass2Timers::secs(&timers.build);
    // Read/decode/simplify/build run on the producer thread and overlap the
    // writer (#264), so these stage sums are core-seconds that overlap the
    // `total` wall time — the writer's own cost is roughly
    // `total - max(producer stages)`, not `total - sum`.
    let writer_busy = total - Duration::from_nanos(recv_wait_ns.get()).as_secs_f64();
    log::debug!(
        "[profile] level {} ({}, {} rows): total={:.2}s read={:.2}s decode={:.2}s \
         simplify={:.2}s build={:.2}s writer_busy={:.2}s (read/decode/simplify/build \
         overlap the writer)",
        level_idx,
        if ctx.verbatim { "verbatim" } else { "simplify" },
        rows.get(),
        total,
        read_s,
        decode_s,
        simplify_s,
        build_s,
        writer_busy,
    );
    let fallbacks = full_resolution_fallback_count() - fallbacks_before;
    if fallbacks > 0 {
        log::debug!(
            "[profile] level {level_idx}: {fallbacks} feature(s) kept at full \
             resolution (invalid RDP candidate after all epsilon retries)"
        );
    }
    Ok((outcome, rows.get(), vertices.get(), level_timers))
}

/// Process one input batch for one level: select the level's members from the
/// winner table, decode only their geometries, simplify (unless verbatim), and
/// assemble the output batch. Returns `None` when no member row survives.
pub(super) fn process_level_batch(
    batch: &RecordBatch,
    row_offset: usize,
    ctx: &LevelStreamCtx<'_>,
    timers: &Pass2Timers,
) -> Result<Option<(RecordBatch, usize)>, ConvertError> {
    let n = batch.num_rows();
    let t_decode = Instant::now();
    let selected: Vec<usize> = (0..n)
        .filter(|&i| {
            let g = row_offset + i;
            // Coalesced level: line rows bypass the winner table entirely —
            // only surviving chain reps are emitted (with merged geometry).
            if let Some(table) = ctx.coalesce_table {
                if ctx.kinds.expect("kinds present when coalescing")[g] == FeatureKind::Line {
                    return table.contains_key(&g);
                }
            }
            ctx.is_member(g)
        })
        .collect();
    if selected.is_empty() {
        return Ok(None);
    }

    // Decode only the selected rows' geometries (take → decode, not
    // decode-all → filter).
    let take_idx = UInt32Array::from(selected.iter().map(|&i| i as u32).collect::<Vec<_>>());
    let geom_taken = take(batch.column(ctx.geom_idx).as_ref(), &take_idx, None)?;
    let schema = batch.schema();
    let gfield = schema.field(ctx.geom_idx);
    let garr = from_arrow_array(geom_taken.as_ref(), gfield)
        .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(selected.len());
    extract_geometries_from_array(garr.as_ref(), &mut geoms)?;
    Pass2Timers::add(&timers.decode, t_decode);

    let t_simplify = Instant::now();
    let mut kept_idx: Vec<usize> = Vec::with_capacity(selected.len());
    let mut verts = 0usize;

    let kept_geoms: Vec<Geometry<f64>> = if ctx.verbatim {
        for (g, &i) in geoms.iter().zip(&selected) {
            verts += count_vertices(g);
            kept_idx.push(i);
        }
        geoms
    } else {
        // Simplification is >95% of pass-2 wall time (H3(c) profile) and
        // embarrassingly parallel per feature. `par_iter().map().collect()`
        // preserves within-batch order, so the output stays byte-identical to
        // the serial path; the writer (our single caller) remains
        // single-threaded, and memory stays bounded by one read batch.
        // Chain reps substitute their merged, already-simplified geometry
        // (simplified once in `build_level_coalesce_table`, identically to
        // the in-memory path).
        //
        // Cascading (#218): a non-empty `cascade_chain` folds canonical
        // geometry fine→coarse down to this level. This per-level recompute
        // is O(levels) per feature — it exists for the Serial reference
        // engine; the pipelined engine shares fold prefixes across levels
        // via `process_batch_cascade` and computes identical results.
        let simplified: Vec<Simplified> = geoms
            .par_iter()
            .zip(&selected)
            .map(|(g, &i)| {
                if let Some((merged, _)) = ctx.coalesce_table.and_then(|t| t.get(&(row_offset + i)))
                {
                    Simplified::Keep(merged.clone())
                } else if ctx.is_carrier_row(row_offset + i) {
                    // #384: a carrier stands in for its neighbourhood's
                    // dropped area as one placeholder square.
                    carrier_square(g, ctx.gsd_m, ctx.crs, ctx.simplify)
                        .map_or(Simplified::Dropped, Simplified::Keep)
                } else if !ctx.cascade_chain.is_empty() {
                    simplify_cascade(g, ctx.cascade_chain, ctx.crs, ctx.simplify)
                } else {
                    simplify_step(g, ctx.gsd_m, ctx.crs, ctx.simplify, ctx.repr)
                }
            })
            .collect();
        let mut out = Vec::with_capacity(selected.len());
        for (s, &i) in simplified.into_iter().zip(&selected) {
            match s {
                Simplified::Keep(s) => {
                    verts += count_vertices(&s);
                    kept_idx.push(i);
                    out.push(s);
                }
                Simplified::Dropped => {}
            }
        }
        if out.is_empty() {
            Pass2Timers::add(&timers.simplify, t_simplify);
            return Ok(None);
        }
        out
    };
    Pass2Timers::add(&timers.simplify, t_simplify);

    let t_build = Instant::now();
    let out_batch = assemble_level_batch(batch, row_offset, ctx, &kept_idx, &kept_geoms)?;
    Pass2Timers::add(&timers.build, t_build);
    Ok(Some((out_batch, verts)))
}

/// Assemble one level's output batch from kept row indices + geometries:
/// project source columns, splice the geometry column, then append
/// cluster / coalesced-count columns. Shared by [`process_level_batch`] and
/// [`process_batch_cascade`].
///
/// `kept_geoms` is borrowed and generic over `Borrow<Geometry<f64>>` (#499):
/// [`process_batch_cascade`] shares one allocation across every ladder level
/// whose step removed nothing and so passes `&[Arc<Geometry<f64>>]`, while
/// [`process_level_batch`] owns its geometries outright and passes
/// `&[Geometry<f64>]`. Note that the owned side is NOT a Serial-only path:
/// the pipelined production engine streams its finest level through
/// `write_level_streaming` -> `process_level_batch` as well. Assembly must not
/// undo either caller's choice by deep-cloning or re-wrapping on the way into
/// the Arrow builder.
fn assemble_level_batch(
    batch: &RecordBatch,
    row_offset: usize,
    ctx: &LevelStreamCtx<'_>,
    kept_idx: &[usize],
    kept_geoms: &[impl std::borrow::Borrow<Geometry<f64>>],
) -> Result<RecordBatch, ConvertError> {
    let mut out_batch = build_level_batch(
        ctx.source_schema,
        batch,
        ctx.non_geom_cols,
        ctx.geom_idx,
        kept_idx,
        kept_geoms,
    )?;
    if ctx.cluster_enabled || ctx.coalesce_enabled {
        // Cluster/coalesce-table keys are global row indices; kept_idx is
        // batch-local.
        let globals: Vec<usize> = kept_idx.iter().map(|&i| row_offset + i).collect();
        if ctx.cluster_enabled {
            out_batch = apply_cluster_columns(
                out_batch,
                ctx.cluster_schema,
                &globals,
                ctx.cluster_table,
                ctx.acc_cols,
            )?;
        }
        if ctx.coalesce_enabled {
            out_batch =
                apply_coalesced_count(out_batch, ctx.out_schema, &globals, ctx.coalesce_table)?;
        }
    }
    Ok(out_batch)
}

/// One cascade-fold step's result, sharing (`Arc`) the kept geometry instead
/// of owning a deep clone (#499).
///
/// The fold in [`process_batch_cascade`] used to store `Simplified` directly,
/// which owns a `Geometry<f64>` — cheap for one step, but the fold pushes one
/// entry per feature *per level*, and adjacent ladder levels commonly
/// simplify to the exact same geometry (small polygons already at minimum
/// vertex count, point bands that pass a point through untouched). Wrapping
/// the kept geometry in `Arc` lets every level after the first unchanged step
/// reuse that one allocation (`Arc::clone`, a refcount bump) instead of
/// retaining its own copy, cutting peak RSS and allocator traffic on
/// FTW-shaped data without changing a single output byte.
enum SharedStep {
    /// Geometry survives at this level — shared with whichever earlier fold
    /// step (or the canonical decode) first produced this exact value.
    Keep(Arc<Geometry<f64>>),
    /// Geometry is not meaningful at this level (mirrors
    /// [`Simplified::Dropped`]).
    Dropped,
}

/// Pipelined-engine batch processor for cascading simplification (#218).
///
/// Instead of every level independently decoding canonical geometry and
/// simplifying it from full resolution ([`process_level_batch`] per level),
/// this decodes each batch's member geometries **once**, computes each
/// feature's fine→coarse simplification fold **once** (level *k* consumes
/// level *k+1*'s output — the shared prefix is what the per-level path
/// recomputes), then assembles every level's output batch.
///
/// Bit-identical to running [`process_level_batch`] per level with the same
/// ctxs (the Serial reference): the incremental fold steps through exactly
/// the per-level `cascade_chain` GSD sequence, and each level's rows are
/// gathered in the same ascending batch order the per-level selection uses.
///
/// `ctxs` must be the pipelined engine's buffered slice: all non-verbatim
/// duplicating levels, coarse→fine.
pub(super) fn process_batch_cascade(
    batch: &RecordBatch,
    row_offset: usize,
    ctxs: &[LevelStreamCtx<'_>],
    timers: &Pass2Timers,
) -> Result<Vec<Option<(RecordBatch, usize)>>, ConvertError> {
    let Some(finest) = ctxs.last() else {
        return Ok(Vec::new());
    };
    debug_assert!(ctxs.iter().all(|c| c.duplicating && !c.verbatim));
    // #541: the chain steps FINER than the finest buffered level — the levels
    // a level-capped coarse job assigns but never materializes. Empty for an
    // uncapped run (there the finest buffered level's chain is exactly its own
    // step), so the fold below is unchanged for every non-sharded build. When
    // it is non-empty the fold walks it first, from canonical geometry, which
    // is precisely what `simplify_cascade` does for the same level on the
    // Serial path — the two must stay in lockstep.
    let prefix: &[CascadeStep] =
        &finest.cascade_chain[..finest.cascade_chain.len().saturating_sub(1)];
    // The incremental fold steps ctx-by-ctx; each level's cascade_chain must
    // be exactly the GSD suffix from the finest buffered level down to it
    // (plus the unmaterialized prefix), or Serial and Pipelined would diverge.
    debug_assert!(ctxs.iter().enumerate().all(|(li, c)| c.cascade_chain.len()
        == ctxs.len() - li + prefix.len()
        && c.cascade_chain.last()
            == Some(&CascadeStep {
                gsd_meters: c.gsd_m,
                repr: c.repr,
            })));
    // Coalesce-table presence is uniform across buffered levels (tables are
    // built for every non-verbatim level or none); the superset selection
    // below relies on it.
    debug_assert!(ctxs
        .iter()
        .all(|c| c.coalesce_table.is_some() == finest.coalesce_table.is_some()));

    let n = batch.num_rows();

    // --- Select the cascade superset: members of the finest buffered level.
    // Coalesced line rows never cascade — each level emits its own chain
    // reps with merged, per-level-simplified geometry instead.
    let t_decode = Instant::now();
    let mut pos_of_row: Vec<u32> = vec![u32::MAX; n];
    let mut selected: Vec<usize> = Vec::with_capacity(n);
    for (i, pos) in pos_of_row.iter_mut().enumerate() {
        let g = row_offset + i;
        if finest.coalesce_table.is_some()
            && finest.kinds.expect("kinds present when coalescing")[g] == FeatureKind::Line
        {
            continue;
        }
        if finest.min_levels[g] <= finest.orig_level
            || ctxs.iter().any(|c| is_carrier(c.carriers, g))
        {
            *pos = u32::try_from(selected.len()).expect("batch rows fit in u32");
            selected.push(i);
        }
    }

    // Decode only the selected rows' geometries, once for all levels. Wrapped
    // in `Arc` immediately (#499): the fold below shares this same
    // allocation via `Arc::clone` for every level a feature survives
    // unchanged, instead of deep-cloning canonical geometry once per level.
    let mut geoms_owned: Vec<Geometry<f64>> = Vec::with_capacity(selected.len());
    if !selected.is_empty() {
        let take_idx = UInt32Array::from(selected.iter().map(|&i| i as u32).collect::<Vec<_>>());
        let geom_taken = take(batch.column(finest.geom_idx).as_ref(), &take_idx, None)?;
        let schema = batch.schema();
        let gfield = schema.field(finest.geom_idx);
        let garr = from_arrow_array(geom_taken.as_ref(), gfield)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
        extract_geometries_from_array(garr.as_ref(), &mut geoms_owned)?;
    }
    let geoms: Vec<Arc<Geometry<f64>>> = geoms_owned.into_iter().map(Arc::new).collect();
    Pass2Timers::add(&timers.decode, t_decode);

    // --- Per-feature incremental fold, fine→coarse, parallel over features.
    // folds[pos][d] is the result at ctxs[len-1-d]; entries stop at the
    // feature's coarsest member level, or earlier once Dropped with only
    // Geometry ctxs remaining (drops are monotone along geometry steps, so
    // a missing depth reads as dropped). Point / Square ctxs (#317 / #279)
    // REVIVE from canonical geometry — see `simplify_cascade`, whose fold
    // this mirrors step-for-step so Serial and Pipelined stay identical.
    //
    // has_band_upto[li]: whether any ctx at index <= li (i.e. this level or
    // a coarser one) carries a non-Geometry representation — the condition
    // under which a dropped fold must keep walking instead of breaking.
    let has_band_upto: Vec<bool> = {
        let mut v = Vec::with_capacity(ctxs.len());
        let mut any = false;
        for c in ctxs.iter() {
            any = any || c.repr != Representation::Geometry;
            v.push(any);
        }
        v
    };
    let t_simplify = Instant::now();
    // Keep step semantics in lock-step with `super::simplify::simplify_cascade`;
    // equivalence is enforced by `overview::convert::tests::pipelined_matches_serial`.
    // Per-feature fold, `(steps, shared_count, total_count)`. `.collect()` on
    // this `zip` (an `IndexedParallelIterator`) preserves ascending `pos`
    // order exactly like the pre-#499 `Vec<Vec<Simplified>>` collect did —
    // load-bearing, since `folds[pos]` below is indexed by that position.
    // The two per-feature counters (#499's `identical_steps` profile metric)
    // are summed in a plain sequential pass after collecting, rather than
    // via a rayon `fold`/`reduce` that could reorder `folds`.
    let per_feature: Vec<(Vec<SharedStep>, u64, u64)> = geoms
        .par_iter()
        .zip(&selected)
        .map(|(g, &i)| {
            let ml = finest.min_levels[row_offset + i];
            let mut out: Vec<SharedStep> = Vec::with_capacity(ctxs.len());
            let mut fold = CascadeFold::new();
            let (mut shared, mut total) = (0u64, 0u64);
            // #541: walk the unmaterialized fine steps first, so the finest
            // buffered ctx folds from the same working geometry a full run
            // would have handed it — the same `CascadeFold` steps
            // `simplify_cascade` takes over the whole chain on the Serial
            // path. Guarded by the same membership test the loop's first
            // iteration applies, so a carrier-only row (not a member anywhere
            // here) pays nothing.
            if !prefix.is_empty() && ml <= finest.orig_level {
                for step in prefix {
                    fold.step(g, step, finest.crs, finest.simplify);
                }
            }
            for (li, ctx) in ctxs.iter().enumerate().rev() {
                if ml > ctx.orig_level {
                    break; // duplicating membership is a contiguous fine suffix
                }
                if !fold.is_alive() && !has_band_upto[li] {
                    break; // only geometry ctxs remain: dropped stays dropped
                }
                let step = CascadeStep {
                    gsd_meters: ctx.gsd_m,
                    repr: ctx.repr,
                };
                match fold.step(g, &step, ctx.crs, ctx.simplify) {
                    FoldStep::Keep { geom, shared: s } => {
                        total += 1;
                        // An unchanged step shares its input's allocation
                        // (#499): a refcount bump, not a retained copy.
                        if s {
                            shared += 1;
                        }
                        out.push(SharedStep::Keep(geom));
                    }
                    FoldStep::Dropped => out.push(SharedStep::Dropped),
                }
            }
            (out, shared, total)
        })
        .collect();
    let mut folds: Vec<Vec<SharedStep>> = Vec::with_capacity(per_feature.len());
    let mut shared_count = 0u64;
    let mut total_count = 0u64;
    for (out, s, t) in per_feature {
        folds.push(out);
        shared_count += s;
        total_count += t;
    }
    timers.record_cascade_steps(shared_count, total_count);
    Pass2Timers::add(&timers.simplify, t_simplify);

    // --- Assemble every level's batch, in the per-level selection's
    // ascending row order (chain reps interleaved by global row index).
    let t_build = Instant::now();
    let results: Vec<Result<Option<(RecordBatch, usize)>, ConvertError>> = ctxs
        .par_iter()
        .enumerate()
        .map(|(li, ctx)| {
            let depth = ctxs.len() - 1 - li;
            let mut kept_idx: Vec<usize> = Vec::new();
            let mut kept_geoms: Vec<Arc<Geometry<f64>>> = Vec::new();
            let mut verts = 0usize;
            for (i, &pos) in pos_of_row.iter().enumerate() {
                let g = row_offset + i;
                if let Some(table) = ctx.coalesce_table {
                    if ctx.kinds.expect("kinds present when coalescing")[g] == FeatureKind::Line {
                        if let Some((merged, _)) = table.get(&g) {
                            verts += count_vertices(merged);
                            kept_idx.push(i);
                            kept_geoms.push(Arc::new(merged.clone()));
                        }
                        continue;
                    }
                }
                if ctx.min_levels[g] <= ctx.orig_level {
                    debug_assert_ne!(pos, u32::MAX, "member row missing from cascade superset");
                    if let Some(SharedStep::Keep(s)) = folds[pos as usize].get(depth) {
                        verts += count_vertices(s);
                        kept_idx.push(i);
                        // Cheap: a refcount bump, not a deep clone — the
                        // whole point of `SharedStep` (#499).
                        kept_geoms.push(Arc::clone(s));
                    }
                } else if ctx.is_carrier_row(g) {
                    // #384: not a member, but the carrier of its cell's
                    // dropped area — one placeholder square.
                    debug_assert_ne!(pos, u32::MAX, "carrier row missing from cascade superset");
                    if let Some(sq) = carrier_square(
                        geoms[pos as usize].as_ref(),
                        ctx.gsd_m,
                        ctx.crs,
                        ctx.simplify,
                    ) {
                        verts += count_vertices(&sq);
                        kept_idx.push(i);
                        kept_geoms.push(Arc::new(sq));
                    }
                }
            }
            if kept_idx.is_empty() {
                return Ok(None);
            }
            let out_batch = assemble_level_batch(batch, row_offset, ctx, &kept_idx, &kept_geoms)?;
            Ok(Some((out_batch, verts)))
        })
        .collect();
    Pass2Timers::add(&timers.build, t_build);

    let mut per_level = Vec::with_capacity(results.len());
    for res in results {
        per_level.push(res?);
    }
    Ok(per_level)
}

// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    use geo::{LineString, Point, Polygon};

    /// `LevelPlan` lives in `convert.rs` and `write_input_with_f64` in the
    /// shared `testutil` module — neither is re-imported at the top of this
    /// module (only the items `run_pass1` itself needs are), so pull them in
    /// via the grandparent (`overview`) module directly.
    use super::super::convert::LevelPlan;
    use super::super::testutil::write_input_with_f64;

    /// A mix of points, lines, and polygons, spread out so every row is its
    /// own coarse-level cell winner and no feature collapses during
    /// simplification (that would make an equivalence test brittle for the
    /// wrong reason — collapse thresholds, not chunking, deciding the
    /// output).
    fn mixed_geometries(n_points: usize, n_lines: usize, n_polys: usize) -> Vec<Geometry<f64>> {
        let mut geoms = Vec::new();
        for i in 0..n_points {
            geoms.push(Geometry::Point(Point::new(i as f64 * 3.0, i as f64 * 2.0)));
        }
        for i in 0..n_lines {
            let base = 100.0 + i as f64 * 10.0;
            let ls = LineString::from(vec![(base, 0.0), (base + 5.0, 3.0), (base + 9.0, 1.0)]);
            geoms.push(Geometry::LineString(ls));
        }
        for i in 0..n_polys {
            let cx = -80.0 + i as f64 * 12.0;
            let cy = -40.0 - i as f64 * 5.0;
            let half = 2.0 + i as f64;
            let ext = LineString::from(vec![
                (cx - half, cy - half),
                (cx + half, cy - half),
                (cx + half, cy + half),
                (cx - half, cy + half),
                (cx - half, cy - half),
            ]);
            geoms.push(Geometry::Polygon(Polygon::new(ext, vec![])));
        }
        geoms
    }

    /// Pure-arithmetic mirror of the chunk-splitting loop in
    /// `run_pass1_with_chunk_rows`'s consumer (`chunk_rows.min(remaining)`
    /// repeated per batch until it's consumed, one batch per
    /// `read_batch_size`-row group except a possibly-shorter last one) — the
    /// total number of `scan_chunk` calls a `(num_rows, read_batch_size,
    /// chunk_rows)` combination produces.
    ///
    /// Used by tests to assert a fixture's parameters actually exercise more
    /// than one chunk per batch, deterministically. A runtime counter would
    /// need to be shared (test-only) global state, which is unsound under
    /// `cargo test`'s default cross-test parallelism — other tests call
    /// `run_pass1` concurrently in the same process, so a shared counter
    /// would be racy and occasionally flaky. Pure arithmetic has no such
    /// hazard.
    fn total_pass1_chunks(num_rows: usize, read_batch_size: usize, chunk_rows: usize) -> usize {
        let read_batch_size = read_batch_size.max(1);
        let chunk_rows = chunk_rows.max(1);
        let mut total = 0usize;
        let mut remaining = num_rows;
        while remaining > 0 {
            let batch_len = remaining.min(read_batch_size);
            total += batch_len.div_ceil(chunk_rows);
            remaining -= batch_len;
        }
        total
    }

    /// #543: the pass-1 memory-floor preflight fires from footer-derived row
    /// counts alone, before pass 1 (or even `stage_input_pass0`) touches a
    /// single data page — proven with a real fixture file and a mocked limit
    /// (the #509-style `_with_memory_limit` test seam), the way #509's own
    /// `build_writer_options_with_ceiling` proved its ceiling preflight.
    #[test]
    fn convert_preflight_fails_fast_under_a_tiny_memory_limit() {
        let geoms: Vec<Option<Geometry<f64>>> =
            mixed_geometries(10, 0, 0).into_iter().map(Some).collect();
        let values: Vec<f64> = vec![1.0; geoms.len()];
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input_with_f64(tin.path(), &geoms, "rank", &values);

        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 5,
            },
            ..Default::default()
        };
        let source = ConvertSource::resolve_path(tin.path()).unwrap();

        // A generous mocked limit fits comfortably.
        convert_preflight_with_memory_limit(&source, &options, hard_limit(1 << 40), false)
            .expect("10 rows must fit a 1 TiB mocked limit");

        // A 1-byte mocked hard limit cannot possibly fit 10 rows' feature
        // table — this must fail from the footer row count alone, never
        // having opened a data page (the fixture is tiny; if this reached
        // pass 1 it would simply succeed, silently defeating the test).
        match convert_preflight_with_memory_limit(&source, &options, hard_limit(1), false) {
            Err(
                err @ ConvertError::Pass1MemoryFloorExceeded {
                    rows: 10,
                    estimated_bytes: 640,
                    limit_bytes: 1,
                },
            ) => {
                let msg = err.to_string();
                for want in [
                    "10 input row(s)",
                    "cgroup hard memory limit (memory.max)",
                    "TYLERTOO_SKIP_MEMORY_PREFLIGHT=1",
                ] {
                    assert!(msg.contains(want), "missing {want:?}: {msg}");
                }
            }
            Err(err) => panic!("wrong error: {err}"),
            Ok(_) => panic!("10 rows must not fit a 1-byte mocked limit"),
        }

        // The escape hatch downgrades it to a warning.
        convert_preflight_with_memory_limit(&source, &options, hard_limit(1), true)
            .expect("the skip hatch must downgrade the hard error");

        // #543 review (S1-2): under a per-feature --bbox the footer count is
        // only an upper bound, so the same tiny hard limit only warns.
        let bbox_options = ConvertOptions {
            bbox: Some([-180.0, -90.0, 180.0, 90.0]),
            ..options.clone()
        };
        convert_preflight_with_memory_limit(&source, &bbox_options, hard_limit(1), false)
            .expect("a --bbox extract must never hard-error on an upper-bound row count");
    }

    // `Option` because every call site feeds an `Option<MemoryLimit>` parameter.
    #[allow(clippy::unnecessary_wraps)]
    fn hard_limit(bytes: u64) -> Option<super::super::pipeline::MemoryLimit> {
        Some(super::super::pipeline::MemoryLimit {
            bytes,
            source: super::super::pipeline::MemoryLimitSource::CgroupMax,
        })
    }

    /// #543 review (S1-1): the production preflight must not touch
    /// `pipeline::available_memory_bytes` — its process-wide cache is meant
    /// to hold post-pass-1 headroom, and a pre-scan call would freeze a
    /// near-empty-cgroup figure for every later `auto` decision. Counted
    /// per thread, so parallel tests warming the cache cannot race this.
    #[test]
    fn convert_preflight_does_not_touch_the_available_memory_cache() {
        let geoms: Vec<Option<Geometry<f64>>> =
            mixed_geometries(10, 0, 0).into_iter().map(Some).collect();
        let values: Vec<f64> = vec![1.0; geoms.len()];
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input_with_f64(tin.path(), &geoms, "rank", &values);
        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 5,
            },
            ..Default::default()
        };
        let source = ConvertSource::resolve_path(tin.path()).unwrap();

        let before = super::super::pipeline::available_memory_bytes_calls_on_this_thread();
        convert_preflight(&source, &options).expect("10 rows fit any real box");
        assert_eq!(
            super::super::pipeline::available_memory_bytes_calls_on_this_thread(),
            before,
            "convert_preflight must use the uncached probe"
        );
    }

    /// #543: a `--plan` replay never builds the pass-1 feature table (it
    /// re-addresses the saved 1-byte/row winner table instead — see
    /// `load_plan_state`), so the memory-floor preflight must not apply to
    /// it — checked here against an impossibly small mocked limit that would
    /// otherwise certainly fail.
    #[test]
    fn convert_preflight_skips_memory_check_for_plan_replay() {
        let geoms: Vec<Option<Geometry<f64>>> =
            mixed_geometries(10, 0, 0).into_iter().map(Some).collect();
        let values: Vec<f64> = vec![1.0; geoms.len()];
        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input_with_f64(tin.path(), &geoms, "rank", &values);

        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 5,
            },
            plan: Some(std::path::PathBuf::from("/nonexistent/convert.plan")),
            ..Default::default()
        };
        let source = ConvertSource::resolve_path(tin.path()).unwrap();

        convert_preflight_with_memory_limit(&source, &options, hard_limit(1), false)
            .expect("a --plan replay must skip the pass-1 memory preflight entirely");
    }

    /// Run [`run_pass1_with_chunk_rows`] over a fixture file with a fresh
    /// [`convert_preflight`] each time (mirrors how `convert_streaming_strategy`
    /// prepares its inputs).
    fn run_pass1_for_test(path: &Path, options: &ConvertOptions, chunk_rows: usize) -> Pass1Output {
        let source = ConvertSource::resolve_path(path).unwrap();
        let pre = convert_preflight(&source, options).unwrap();
        run_pass1_with_chunk_rows(
            &source,
            &pre.input_schema,
            pre.geom_idx,
            &pre.options,
            &pre.acc_cols,
            pre.selected_row_groups.as_ref(),
            pre.bbox_units.as_ref(),
            pre.bound_filter.as_ref(),
            chunk_rows,
        )
        .unwrap()
    }

    /// Field-by-field [`AssignFeature`] comparison ([`AssignFeature`] has no
    /// `PartialEq` — it isn't needed outside tests, and deriving it here would
    /// mean editing `assign.rs`, outside this PR's file scope).
    fn assert_features_eq(a: &[AssignFeature], b: &[AssignFeature]) {
        assert_eq!(a.len(), b.len(), "feature count differs");
        for (i, (x, y)) in a.iter().zip(b).enumerate() {
            assert_eq!(x.index, y.index, "feature {i}: index differs");
            assert_eq!(x.bbox, y.bbox, "feature {i}: bbox differs");
            assert_eq!(x.kind, y.kind, "feature {i}: kind differs");
            assert_eq!(x.sort_key, y.sort_key, "feature {i}: sort_key differs");
            assert_eq!(
                x.entry_level, y.entry_level,
                "feature {i}: entry_level differs"
            );
        }
    }

    /// Full [`Pass1Output`] equivalence: every field a caller of `run_pass1`
    /// depends on, EXCEPT `pass1_stage_secs` (wall-clock profiling data,
    /// expected to differ between a serial and a chunked/parallel run).
    fn assert_pass1_outputs_eq(a: &Pass1Output, b: &Pass1Output) {
        assert_eq!(a.num_rows, b.num_rows, "num_rows differs");
        assert_eq!(a.skipped_rows, b.skipped_rows, "skipped_rows differs");
        assert_eq!(a.geom_bytes, b.geom_bytes, "geom_bytes differs");
        assert_eq!(a.provenance, b.provenance, "ranking provenance differs");
        assert_eq!(a.acc_values, b.acc_values, "acc_values differ");
        assert_features_eq(&a.features, &b.features);
        assert_eq!(a.areas, b.areas, "areas differ");
        match (&a.coalesce, &b.coalesce) {
            (None, None) => {}
            (Some(x), Some(y)) => {
                assert_eq!(x.rows, y.rows, "coalesce rows differ");
                assert_eq!(x.sort_keys, y.sort_keys, "coalesce sort_keys differ");
                assert_eq!(x.groups, y.groups, "coalesce groups differ");
                assert_eq!(x.geoms, y.geoms, "coalesce geoms differ");
            }
            _ => panic!("coalesce presence differs between serial and chunked runs"),
        }
    }

    /// #460: a chunk size of `usize::MAX` (one chunk per batch — the pre-#460
    /// shape, one big `scan_chunk` call) must produce byte-identical
    /// [`Pass1Output`] to a heavily chunked run (`chunk_rows: 7`, several
    /// chunks per batch, `read_batch_size: 7` too, so chunk AND batch
    /// boundaries both cut across the fixture repeatedly).
    #[test]
    fn pass1_parallel_matches_serial_on_fixture() {
        let geoms = mixed_geometries(23, 9, 7); // points, lines, polygons: 39 rows
        let n = geoms.len();
        let values: Vec<f64> = (0..n).map(|i| (n - i) as f64).collect();
        let opt_geoms: Vec<Option<Geometry<f64>>> = geoms.into_iter().map(Some).collect();

        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input_with_f64(tin.path(), &opt_geoms, "rank", &values);

        // read_batch_size (16) > chunk_rows (5): every batch splits into
        // SEVERAL chunks. (#460 review: read_batch_size == chunk_rows made
        // every batch exactly one chunk on both sides of the comparison
        // below, comparing the chunked path to itself with zero multi-chunk
        // coverage of the two most order-sensitive merges — line coalesce
        // scratch and #384 areas — hence the `total_pass1_chunks` assertion
        // that pins this down.)
        const READ_BATCH_SIZE: usize = 16;
        const CHUNK_ROWS: usize = 5;
        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 9,
            },
            sort_key: Some("rank".to_string()),
            read_batch_size: READ_BATCH_SIZE,
            // #384 tiny-polygon accumulator: exercises the `want_areas` path
            // (per-feature polygon-area collection) through the chunked scan.
            simplify: SimplifyOptions {
                collapse: CollapseMode::Square,
                ..Default::default()
            },
            ..Default::default()
        };

        let serial = run_pass1_for_test(tin.path(), &options, usize::MAX);
        let chunked = run_pass1_for_test(tin.path(), &options, CHUNK_ROWS);

        assert_pass1_outputs_eq(&serial, &chunked);
        assert!(
            !serial.features.is_empty(),
            "fixture must produce at least one feature"
        );
        assert!(
            serial.coalesce.is_some(),
            "fixture's lines must produce a coalesce scratch (coalesce_lines defaults on)"
        );

        // Guard against the comparison above going vacuous: the chunked run
        // must actually have split every batch into more than one chunk, not
        // just matched the one-chunk-per-batch serial shape by coincidence.
        let serial_chunks = total_pass1_chunks(n, READ_BATCH_SIZE, usize::MAX);
        let chunked_chunks = total_pass1_chunks(n, READ_BATCH_SIZE, CHUNK_ROWS);
        assert!(
            chunked_chunks > serial_chunks,
            "chunk_rows={CHUNK_ROWS} must produce more chunks than \
             chunk_rows=usize::MAX (one per batch): serial={serial_chunks} \
             chunked={chunked_chunks} over {n} rows / {READ_BATCH_SIZE}-row \
             batches"
        );
    }

    /// #460: nulls, non-finite geometries, and attribute-filtered rows are
    /// interleaved so at least one of every skip category lands in the
    /// middle of a chunk (`chunk_rows: 3`) as well as straddling a chunk
    /// boundary and a batch boundary (`read_batch_size: 5`). Every kept
    /// feature's global row `index` — the thing `Priority::beats`'s
    /// `stable_hash(index)` tie-break depends on — must match the serial
    /// (`usize::MAX` chunk) run exactly.
    #[test]
    fn pass1_chunking_preserves_row_indices_with_skipped_geometries() {
        let mut geoms: Vec<Option<Geometry<f64>>> = Vec::new();
        let mut values: Vec<f64> = Vec::new();
        for i in 0..24 {
            match i % 6 {
                // Null geometry: skipped (H4), row index still advances.
                0 => {
                    geoms.push(None);
                    values.push(10.0);
                }
                // Non-finite coordinate: `scan_feature` rejects it, skipped.
                1 => {
                    geoms.push(Some(Geometry::Point(Point::new(f64::NAN, 1.0))));
                    values.push(10.0);
                }
                // Valid geometry, but the attribute filter drops it (rank <=
                // 5): NOT counted in `skipped_rows`, but still no feature.
                2 => {
                    geoms.push(Some(Geometry::Point(Point::new(i as f64, i as f64 * 0.5))));
                    values.push(1.0);
                }
                // Valid, kept.
                _ => {
                    geoms.push(Some(Geometry::Point(Point::new(
                        i as f64 * 2.0,
                        -(i as f64),
                    ))));
                    values.push(10.0);
                }
            }
        }

        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input_with_f64(tin.path(), &geoms, "rank", &values);

        const READ_BATCH_SIZE: usize = 5;
        const CHUNK_ROWS: usize = 3;
        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 8,
            },
            filter: Some("rank > 5.0".to_string()),
            read_batch_size: READ_BATCH_SIZE,
            ..Default::default()
        };

        let n = geoms.len();
        let serial = run_pass1_for_test(tin.path(), &options, usize::MAX);
        let chunked = run_pass1_for_test(tin.path(), &options, CHUNK_ROWS);

        assert_pass1_outputs_eq(&serial, &chunked);

        // Sanity: the fixture actually exercises all three skip categories,
        // so the equivalence assertion above is meaningful, not vacuous.
        assert!(
            serial.skipped_rows > 0,
            "fixture must include null/non-finite rows"
        );
        assert!(
            serial.features.len() + serial.skipped_rows < serial.num_rows,
            "fixture must also include attribute-filtered rows (not counted \
             in skipped_rows)"
        );

        // Guard against the comparison above going vacuous (#460 review).
        let serial_chunks = total_pass1_chunks(n, READ_BATCH_SIZE, usize::MAX);
        let chunked_chunks = total_pass1_chunks(n, READ_BATCH_SIZE, CHUNK_ROWS);
        assert!(
            chunked_chunks > serial_chunks,
            "chunk_rows={CHUNK_ROWS} must produce more chunks than \
             chunk_rows=usize::MAX (one per batch): serial={serial_chunks} \
             chunked={chunked_chunks} over {n} rows / {READ_BATCH_SIZE}-row \
             batches"
        );
    }
    /// Hand-built GeoParquet with a WKB geometry column whose value at
    /// `bad_row` is a zero-length (undecodable) payload and whose every other
    /// row is a valid point — the only way to force a geometry DECODE error
    /// at a chosen row index (the geoarrow builder `write_input*` uses cannot
    /// emit a corrupt value). Mirrors `hostile::empty_wkb_value_errors_typed`.
    fn write_input_with_corrupt_wkb(path: &Path, n: usize, bad_row: usize) {
        use arrow_array::{BinaryArray, Int64Array};
        use arrow_schema::DataType;
        use parquet::arrow::ArrowWriter;
        use parquet::file::metadata::KeyValue;
        use std::sync::Arc;

        fn point_wkb(x: f64, y: f64) -> Vec<u8> {
            let mut v = Vec::with_capacity(21);
            v.push(1u8); // little-endian
            v.extend_from_slice(&1u32.to_le_bytes()); // wkbPoint
            v.extend_from_slice(&x.to_le_bytes());
            v.extend_from_slice(&y.to_le_bytes());
            v
        }

        let payloads: Vec<Vec<u8>> = (0..n)
            .map(|i| {
                if i == bad_row {
                    Vec::new()
                } else {
                    point_wkb((i % 179) as f64 * 0.5 - 40.0, (i % 83) as f64 * 0.5 - 20.0)
                }
            })
            .collect();

        let mut md = std::collections::HashMap::new();
        md.insert(
            "ARROW:extension:name".to_string(),
            "geoarrow.wkb".to_string(),
        );
        let geom_field = Field::new("geometry", DataType::Binary, true).with_metadata(md);
        let schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(geom_field),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
                Arc::new(BinaryArray::from_iter_values(payloads.iter())),
            ],
        )
        .unwrap();

        let file = std::fs::File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.append_key_value_metadata(KeyValue::new(
            "geo".to_string(),
            r#"{"version":"1.1.0","primary_column":"geometry","columns":{"geometry":{"encoding":"WKB","geometry_types":[]}}}"#
                .to_string(),
        ));
        writer.close().unwrap();
    }

    /// #460 review (S3-a): a geometry decode failure must name the row range
    /// it came from in GLOBAL row terms. `scan_chunk` sees a sliced column,
    /// so the index `batch_processor` formats is chunk-local — row 1500 of a
    /// 3000-row input reported as "index 476" (1024 + 476) before the fix,
    /// which sends a user looking at the wrong row of their file.
    #[test]
    fn pass1_decode_error_names_global_row_range() {
        const N: usize = 3000;
        const BAD_ROW: usize = 1500;
        const CHUNK_ROWS: usize = 1024;

        let tin = tempfile::NamedTempFile::new().unwrap();
        write_input_with_corrupt_wkb(tin.path(), N, BAD_ROW);

        let options = ConvertOptions {
            levels: LevelPlan::ZoomRange {
                min_zoom: 1,
                max_zoom: 6,
            },
            read_batch_size: 8192, // one batch: the chunk base is the only offset
            ..Default::default()
        };
        let source = ConvertSource::resolve_path(tin.path()).unwrap();
        let pre = convert_preflight(&source, &options).unwrap();
        let res = run_pass1_with_chunk_rows(
            &source,
            &pre.input_schema,
            pre.geom_idx,
            &pre.options,
            &pre.acc_cols,
            pre.selected_row_groups.as_ref(),
            pre.bbox_units.as_ref(),
            pre.bound_filter.as_ref(),
            CHUNK_ROWS,
        );
        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("a zero-length WKB value must fail the pass-1 decode"),
        };
        let msg = err.to_string();

        // The chunk holding row 1500 is rows 1024..2048 of the file.
        let lo = BAD_ROW - BAD_ROW % CHUNK_ROWS;
        let hi = (lo + CHUNK_ROWS).min(N);
        assert!(
            msg.contains(&format!("rows {lo}..{hi}")),
            "decode error must name the global row range {lo}..{hi}; got: {msg}"
        );
        // The range must actually localize the failure (one chunk, and the
        // bad row inside it) — "rows 0..3000" would satisfy the substring
        // above while telling the user nothing.
        assert!(hi - lo <= CHUNK_ROWS && (lo..hi).contains(&BAD_ROW));
        // The chunk-local index the underlying decoder reports is left as-is
        // on purpose (rebasing it would put row arithmetic back inside
        // `scan_chunk`); the range prefix is what makes it interpretable.
        assert!(
            msg.contains(&format!("index {}", BAD_ROW - lo)),
            "expected the (chunk-local) decoder index to survive the wrap; got: {msg}"
        );
    }

    fn ranking_provenance() -> RankingProvenance {
        RankingProvenance {
            mode: "size-fallback".to_string(),
            column: None,
            ranks: None,
            unknown_rank: None,
        }
    }

    fn three_levels() -> Vec<LevelSpec> {
        vec![
            LevelSpec::new(100.0, Some(2)),
            LevelSpec::new(50.0, Some(4)),
            LevelSpec::new(10.0, Some(6)),
        ]
    }

    /// #507: when the projected row-group count already fits the ceiling,
    /// `--row-group-size` passes through unchanged.
    #[test]
    fn build_writer_options_leaves_cap_alone_when_it_fits() {
        let options = ConvertOptions::default();
        let counts = [10usize, 10, 10];
        let opts = build_writer_options_with_ceiling(
            three_levels(),
            &[100.0, 50.0, 10.0],
            &counts,
            Crs::Epsg4326,
            ranking_provenance(),
            &[],
            &options,
            32_000,
        )
        .unwrap();
        assert_eq!(opts.max_row_group_size, options.max_row_group_size);
    }

    /// #507's core scenario: a base cap that would blow the (mocked, tiny)
    /// ceiling is auto-scaled up instead of being carried through to the
    /// writer verbatim, where it would eventually fail mid-write hours in.
    #[test]
    fn build_writer_options_autoscales_cap_past_a_tiny_ceiling() {
        let options = ConvertOptions {
            max_row_group_size: 1,
            ..ConvertOptions::default()
        };
        let counts = [20usize, 20, 20]; // 60 row groups at cap 1
        let ceiling = 5;
        let opts = build_writer_options_with_ceiling(
            three_levels(),
            &[100.0, 50.0, 10.0],
            &counts,
            Crs::Epsg4326,
            ranking_provenance(),
            &[],
            &options,
            ceiling,
        )
        .unwrap();
        assert!(
            opts.max_row_group_size > 1,
            "cap should have been raised, got {}",
            opts.max_row_group_size
        );
        let level_zooms: Vec<Option<u8>> = opts.levels.iter().map(|l| l.zoom).collect();
        let finest_zoom = opts.levels.last().and_then(|l| l.zoom);
        let projected = super::super::writer::projected_row_groups(
            &counts,
            opts.max_row_group_size,
            opts.row_group_size_policy,
            &level_zooms,
            finest_zoom,
        );
        assert!(
            projected <= ceiling,
            "projected {projected} > ceiling {ceiling}"
        );
    }

    /// #507: when even an unbounded cap can't help (more non-empty levels
    /// than the ceiling allows), the preflight errors out before pass 2
    /// opens the output file instead of writing a file the parquet crate
    /// will later reject.
    #[test]
    fn build_writer_options_errors_when_autoscaling_cannot_fit() {
        let options = ConvertOptions::default();
        let counts = [1usize, 1, 1];
        let ceiling = 1; // 3 non-empty levels > ceiling of 1
        let err = build_writer_options_with_ceiling(
            three_levels(),
            &[100.0, 50.0, 10.0],
            &counts,
            Crs::Epsg4326,
            ranking_provenance(),
            &[],
            &options,
            ceiling,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ConvertError::RowGroupCeilingUnreachable {
                levels: 3,
                ceiling: 1
            }
        ));
    }
}
