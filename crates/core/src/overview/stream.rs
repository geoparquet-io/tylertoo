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
//!   auto-detection. [`assign_levels`] and [`apply_density_budget`] then run
//!   over those (the assign engine's own state is `O(occupied cells)` per
//!   level), producing the **winner table**: one `min_level` byte per feature.
//! - **Pass 2** re-reads the (seekable) input once **per level**, coarse →
//!   fine: each batch is filtered against the winner table, simplified for the
//!   level (non-canonical duplicating levels only), and handed straight to the
//!   [`OverviewWriter`]. Nothing is retained across batches.
//!
//! Peak memory is `O(read batch + winner tables)`: the winner table is 1 byte
//! per feature; pass 1 additionally holds the `AssignFeature` vector (~48
//! bytes/feature) and per-candidate ranking keys (16 bytes/feature) while the
//! assignment runs, all freed before pass 2. Residual `O(N)` state is
//! therefore ~50–80 bytes per input feature — for 632k features, a few tens of
//! MB — far below the geometry payload the in-memory path holds.
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
use std::time::{Duration, Instant};

use crossbeam_channel::bounded;

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Schema, SchemaRef};
use arrow_select::take::take;
use geo::Geometry;
use geoarrow::array::from_arrow_array;
use rayon::prelude::*;

use crate::batch_processor::{extract_geometries_from_array, extract_geometries_opt_from_array};
use crate::input_set::{ConvertSource, ReadPlan, RowGroupSelection};

use super::assign::{apply_density_budget, assign_levels, AssignFeature, FeatureKind};
use super::cluster::{build_cluster_tables, verify_sum_invariant, ClusterEntry, ClusterTables};
use super::coalesce::CoalesceInput;
use super::convert::{
    append_coalesced_count_field, append_point_count_field, apply_cluster_columns,
    apply_coalesced_count, build_generalization, build_level_batch, build_level_coalesce_table,
    build_source_schema, class_ranking_provenance, coalesce_effective, coalesce_level_chains,
    count_vertices, extract_class_ranks, extract_sort_keys, feature_kind, fill_level_bytes,
    find_geometry_column, geometry_bbox, mixed_geometry_field, overture_road_ranking,
    record_level_outcome, usable_geometry, validate_cluster_schema, validate_coalesce_schema,
    warn_plan_skipped_levels, ClassRanking, CoalesceTable, ConvertError, ConvertOptions,
    ConvertReport, GroupInterner, SkippedLevelReport, KNOWN_ROAD_CLASSES, ROAD_VOCAB_MIN_DISTINCT,
};
use super::level::{Crs, Mode, RankingProvenance};
use super::pipeline;
use super::simplify::{
    full_resolution_fallback_count, simplify_cascade, simplify_for_level, validation_skip_count,
    Simplified, SimplifyOptions,
};
use super::writer::{
    LevelSpec, LevelWriteOutcome, OverviewWriter, OverviewWriterOptions, LEVEL_COLUMN,
};

/// Row-indexed winner-table sentinel for rows with no feature (null, empty,
/// or non-finite geometry — skipped in pass 1). It matches no level in either
/// mode: [`super::convert::MAX_LEVELS`] caps the plan at 255 levels, so the
/// finest level index is at most 254.
const UNASSIGNED_LEVEL: u8 = u8::MAX;

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

pub(crate) fn convert_streaming_strategy(
    source: &ConvertSource,
    output_path: &Path,
    options: &ConvertOptions,
    strategy: Pass2Strategy,
) -> Result<ConvertReport, ConvertError> {
    let start = Instant::now();

    if options.sort_key.is_some() && options.class_ranking.is_some() {
        return Err(ConvertError::RankingConflict);
    }

    // Schema checks (level column, geometry column) — footer-only reads.
    // (For a remote source, #210, the footer is range-fetched once here and
    // cached across the passes below. For a multi-partition source the
    // schema is the validated union schema and the key-value metadata is
    // partition 0's — construction proved all parts agree.)
    let input_schema: SchemaRef = source.schema()?;

    // CRS detection + rejection (spec Q3) — footer metadata only.
    let kv = source.key_value_metadata()?;
    let crs = super::convert::detect_crs_from_kv(kv.as_ref())?;

    // Regional extract (#102): prune input row groups by bbox covering
    // statistics — footer metadata only, no data pages of skipped groups are
    // ever read. The selection is PER PART and every pass reads the same
    // selection in the same part order, so the global row indices addressing
    // the winner tables stay aligned. Groups without stats are kept; the
    // exact per-feature filter in pass 1 guarantees identical output either
    // way.
    let row_groups_total = source.num_row_groups_total()?;
    let bbox_units = options
        .bbox
        .map(|b| super::convert::bbox_to_crs_units(&b, crs));
    let selected_row_groups: Option<RowGroupSelection> = match bbox_units.as_ref() {
        Some(bb) => Some(source.select_row_groups(bb)?),
        None => None,
    };
    let row_groups_read = selected_row_groups
        .as_ref()
        .map_or(row_groups_total, RowGroupSelection::total_selected);
    if selected_row_groups.is_some() {
        log::info!("bbox filter: reading {row_groups_read}/{row_groups_total} input row groups");
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

    if input_schema
        .fields()
        .iter()
        .any(|f| f.name().eq_ignore_ascii_case(LEVEL_COLUMN))
    {
        return Err(ConvertError::LevelColumnPresent);
    }
    let geom_idx = find_geometry_column(&input_schema).ok_or(ConvertError::NoGeometryColumn)?;
    let geom_field = input_schema.field(geom_idx).clone();

    // Clustering schema checks + accumulate column resolution (Q4).
    let acc_cols = validate_cluster_schema(&input_schema, options)?;
    // Coalescing schema check (Q3).
    validate_coalesce_schema(&input_schema, options)?;

    // --- Pass 1: stream → AssignFeatures + resolved ranking. -----------------
    let t_pass1 = Instant::now();
    let Pass1Output {
        mut features,
        provenance: ranking_provenance,
        acc_values,
        coalesce: coalesce_scratch,
        num_rows,
        skipped_rows,
    } = run_pass1(
        source,
        &input_schema,
        geom_idx,
        options,
        &acc_cols,
        selected_row_groups.as_ref(),
        bbox_units.as_ref(),
    )?;
    if skipped_rows > 0 {
        log::warn!(
            "skipping {skipped_rows} of {num_rows} input rows with a null, \
             empty, or non-finite geometry"
        );
    }
    let num_features = features.len();

    // #188 follow-up: count antimeridian-suspect bboxes and warn once.
    let antimeridian_suspect_features = features
        .iter()
        .filter(|f| super::convert::bbox_antimeridian_suspect(&f.bbox, crs))
        .count();
    super::convert::warn_antimeridian_suspects(antimeridian_suspect_features);

    // Stage markers (#242): everything between pass 1 and the writer used to
    // run in total info-level silence — on planet-scale inputs that was tens
    // of minutes with no output.
    log::info!("[convert] scan complete: {num_features} feature(s) from {num_rows} row(s)");
    log::debug!(
        "[profile] pass1 stream+scan: {:.2}s",
        t_pass1.elapsed().as_secs_f64()
    );

    // --- Winner tables (assignment + Q2 density budget). ---------------------
    let level_specs = options.levels.resolve(options.gsd_base)?;
    let level_gsds: Vec<f64> = level_specs.iter().map(|(g, _)| *g).collect();

    let t_assign = Instant::now();
    let assignment = assign_levels(&features, &level_gsds, &options.assign, crs);
    let assign_secs = t_assign.elapsed().as_secs_f64();
    let t_budget = Instant::now();
    let assignment = if options.density.enabled {
        apply_density_budget(
            &assignment,
            &features,
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

    // The feature-parallel winner table (coarsest level per FEATURE, in
    // `features` order) feeds the cluster stage and the per-level counts.
    let feat_min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
    drop(assignment);

    // Cluster tables (Q4): built from the pass-1 features + final winner
    // table, before the O(N) scratch is freed. Memory afterwards is
    // O(non-singleton clusters), carried into pass 2 alongside `min_levels`.
    // Accumulate values are extracted per ROW; the cluster stage indexes them
    // by feature position, so remap through each feature's row index.
    let cluster_tables: Option<ClusterTables> = if options.cluster {
        let ops: Vec<_> = options.accumulate.iter().map(|s| s.op).collect();
        let acc_feat: Vec<Vec<Option<f64>>> = acc_values
            .iter()
            .map(|vals| features.iter().map(|f| vals[f.index]).collect())
            .collect();
        let tables = build_cluster_tables(
            &features,
            &feat_min_levels,
            &level_gsds,
            &options.assign,
            crs,
            &acc_feat,
            &ops,
        );
        // Strict §12.1 accounting: Σ point_count per level == source point
        // count, and no clustered level thins its points to zero.
        verify_sum_invariant(&features, &feat_min_levels, &tables)
            .map_err(ConvertError::ClusterInvariant)?;
        Some(tables)
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
        for f in &features {
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

    // Planned levels with no winners are omitted (§7.3, #211 auto-clamp);
    // record them for the report + warning.
    let mut skipped: Vec<SkippedLevelReport> = level_specs
        .iter()
        .enumerate()
        .filter(|&(l, _)| counts[l] == 0)
        .map(|(l, &(gsd, zoom))| SkippedLevelReport {
            planned_level: l,
            gsd,
            zoom,
        })
        .collect();
    let emitted: Vec<EmitLevel> = level_specs
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
    if emitted.is_empty() {
        return Err(ConvertError::NoData);
    }
    warn_plan_skipped_levels(&skipped, num_features, emitted[0].gsd, emitted[0].zoom);

    // --- Writer setup (identical to the in-memory path). ---------------------
    let geom_name = geom_field.name().clone();
    let geom_out_field = mixed_geometry_field(&geom_name);
    let source_schema = build_source_schema(&input_schema, geom_idx, geom_out_field);
    // Writer schema: base + point_count when clustering (Q4) + coalesced_count
    // when coalescing (Q3).
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

    let writer_levels: Vec<LevelSpec> = emitted
        .iter()
        .map(|e| LevelSpec::new(e.gsd, e.zoom))
        .collect();
    let emitted_gsds: Vec<f64> = emitted.iter().map(|e| e.gsd).collect();
    let mut writer_opts = OverviewWriterOptions::new(options.mode, writer_levels);
    writer_opts.max_row_group_size = options.max_row_group_size;
    writer_opts.row_group_size_policy = options.row_group_size_policy;
    writer_opts.full_column_stats = options.full_column_stats;
    writer_opts.cogp_compat_key = options.cogp_compat_key;
    writer_opts.generalization = Some(build_generalization(
        &emitted_gsds,
        crs,
        options,
        ranking_provenance,
    ));

    let mut writer = OverviewWriter::create(output_path, &out_schema, writer_opts)?;

    let non_geom_cols: Vec<usize> = (0..input_schema.fields().len())
        .filter(|&c| c != geom_idx)
        .collect();

    // --- Pass 2: single-read pipelined engine + canonical streamed last. -----
    let t_pass2 = Instant::now();

    // Prebuild every non-verbatim level's coalesce chain table up front, in
    // parallel: the single read fans each batch to all levels at once, so all
    // levels' tables must exist during the read. Deterministic and keyed by rep
    // row, so this is byte-identical to the former per-level build.
    let coalesce_tables: Vec<Option<CoalesceTable>> = match &coalesce_scratch {
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
    };

    let duplicating = matches!(options.mode, Mode::Duplicating);
    // Cascading (#218): per level, the fine→coarse GSD chain from the finest
    // non-canonical level down to (and including) that level. Chains are
    // built from the emitted plan, so the Serial fold, the pipelined
    // incremental fold, and the in-memory path all step through the same GSD
    // sequence. Empty when cascading does not apply to the level.
    let cascade_chains: Vec<Vec<f64>> = emitted
        .iter()
        .map(|e| {
            let verbatim = matches!(options.mode, Mode::Partitioning) || e.orig as usize == finest;
            if !duplicating || verbatim || !options.simplify.cascade {
                return Vec::new();
            }
            let mut chain: Vec<f64> = emitted
                .iter()
                .filter(|f| (f.orig as usize) < finest && f.orig >= e.orig)
                .map(|f| f.gsd)
                .collect();
            chain.reverse();
            chain
        })
        .collect();
    let ctxs: Vec<LevelStreamCtx> = emitted
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let verbatim = matches!(options.mode, Mode::Partitioning) || e.orig as usize == finest;
            LevelStreamCtx {
                source_schema: &source_schema,
                cluster_schema: &cluster_schema,
                out_schema: &out_schema,
                non_geom_cols: &non_geom_cols,
                geom_idx,
                min_levels: &min_levels,
                orig_level: e.orig,
                duplicating,
                verbatim,
                gsd_m: e.gsd,
                crs,
                simplify: &options.simplify,
                cluster_enabled: options.cluster,
                // Canonical level: singleton clusters, columns verbatim (§2.4).
                cluster_table: cluster_tables
                    .as_ref()
                    .filter(|_| e.orig as usize != finest)
                    .map(|t| &t[e.orig as usize]),
                acc_cols: &acc_cols,
                coalesce_enabled: options.coalesce_lines,
                kinds: kinds.as_deref(),
                coalesce_table: coalesce_tables[i].as_ref(),
                cascade_chain: &cascade_chains[i],
            }
        })
        .collect();

    let n = emitted.len();
    let hints: Vec<usize> = emitted.iter().map(|e| e.hint).collect();

    // Snapshot for the end-of-pass-2 summary: the counter is process-wide,
    // so report the delta from this conversion only (#242).
    let validation_skips_before = validation_skip_count();

    // `(outcome, rows, vertices)` per emitted level, in level order. The
    // outcome distinguishes a written level from one the writer skipped because
    // every candidate collapsed during simplification (#211).
    let level_stats: Vec<(LevelWriteOutcome, usize, usize)> = match strategy {
        // Reference: one in-order re-read per level (pre-#213 behavior).
        Pass2Strategy::Serial => ctxs
            .iter()
            .enumerate()
            .map(|(i, ctx)| {
                write_level_streaming(
                    &mut writer,
                    i,
                    hints[i],
                    source,
                    options.read_batch_size,
                    options.in_flight_batches,
                    selected_row_groups.as_ref(),
                    ctx,
                )
            })
            .collect::<Result<_, _>>()?,
        // Production: buffer levels 0..n-1 from a single read, then stream the
        // finest (verbatim, largest) level last straight into the writer.
        Pass2Strategy::Pipelined => {
            let buffered_rows: usize = hints[..n - 1].iter().sum();
            let backing = pipeline::resolve_backing(options.profile, options.mode, buffered_rows);
            log::info!(
                "[convert] pass 2: building {n} overview level(s) from a \
                 single read (finest level streamed last)"
            );
            let mut stats = if n > 1 {
                pipeline::run_pass2_buffered(
                    &mut writer,
                    &ctxs[..n - 1],
                    &hints[..n - 1],
                    source,
                    options.read_batch_size,
                    selected_row_groups.as_ref(),
                    options.in_flight_batches,
                    backing,
                    &out_schema,
                )?
            } else {
                Vec::new()
            };
            stats.push(write_level_streaming(
                &mut writer,
                n - 1,
                hints[n - 1],
                source,
                options.read_batch_size,
                options.in_flight_batches,
                selected_row_groups.as_ref(),
                &ctxs[n - 1],
            )?);
            stats
        }
    };
    log_validation_skips(validation_skips_before);

    // Fold each emitted level's write outcome into the shared bookkeeping
    // (#211): `record_level_outcome` appends a renumbered `LevelReport` for a
    // written level, or — for a level the writer omitted because every
    // candidate collapsed during simplification — warns and records the plan in
    // `skipped`, exactly like a plan-time omission.
    let mut level_reports = Vec::with_capacity(emitted.len());
    for (e, (outcome, rows, vertices)) in emitted.iter().zip(level_stats) {
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
            &mut skipped,
        );
    }
    skipped.sort_by_key(|s| s.planned_level);
    if level_reports.is_empty() {
        // Every emitted level collapsed at write time: no valid overview file
        // can be produced (`levels` MUST be non-empty, §3.3).
        return Err(ConvertError::NoData);
    }

    log::debug!(
        "[profile] pass2 total: {:.2}s",
        t_pass2.elapsed().as_secs_f64()
    );

    let t_finish = Instant::now();
    let meta = writer.finish()?;
    log::debug!(
        "[profile] writer.finish: {:.2}s",
        t_finish.elapsed().as_secs_f64()
    );
    fill_level_bytes(output_path, &meta, &mut level_reports)?;

    let total_rows: usize = level_reports.iter().map(|l| l.feature_count).sum();
    let total_vertices: usize = level_reports.iter().map(|l| l.vertex_count).sum();
    let total_compressed_bytes: i64 = level_reports.iter().map(|l| l.compressed_bytes).sum();

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
        antimeridian_suspect_features,
        duration_secs: start.elapsed().as_secs_f64(),
        remote_fetch: super::convert::log_remote_fetch(source),
    })
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
struct CoalesceScratch {
    /// Source row index per collected line, ascending input order.
    rows: Vec<usize>,
    /// The lines' decoded geometries, parallel to `rows`.
    geoms: Vec<Geometry<f64>>,
    /// Sort key per line (Q1 ranking), parallel to `rows`; filled after the
    /// ranking tier resolves.
    sort_keys: Vec<Option<f64>>,
    /// Interned class group per line, parallel to `rows`; `None` = no class
    /// ranking active (all lines compatible).
    groups: Option<Vec<u32>>,
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
}

/// Pass 1: stream the input (geometry + ranking/accumulate columns only) and
/// produce the per-feature [`AssignFeature`]s (with resolved sort keys), the
/// ranking provenance block (§3.5), and — when clustering with aggregation —
/// the per-spec source values (parallel to `acc_cols`). Memory: `O(read
/// batch)` transient + `O(N)` small per-feature records.
fn run_pass1(
    source: &ConvertSource,
    input_schema: &Schema,
    geom_idx: usize,
    options: &ConvertOptions,
    acc_cols: &[usize],
    row_groups: Option<&RowGroupSelection>,
    bbox_units: Option<&[f64; 4]>,
) -> Result<Pass1Output, ConvertError> {
    let mut plan = build_rank_plan(input_schema, options)?;

    // Project only the columns pass 1 needs: geometry + ranking candidates +
    // accumulate columns (Q4).
    let mut cols: Vec<usize> = vec![geom_idx];
    match &plan {
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
    // Original schema index → projected batch column index.
    let proj = |orig: usize| cols.binary_search(&orig).expect("projected column");

    // Regional extract (#102): read only the bbox-selected row groups
    // (identical per-part selection in pass 2, keeping row indices aligned).
    let reader = source.open_stream(&ReadPlan {
        batch_size: options.read_batch_size.max(1),
        projection: Some(&cols),
        row_groups,
    })?;

    let mut features: Vec<AssignFeature> = Vec::new();
    let mut num_rows = 0usize;
    let mut skipped_rows = 0usize;
    let mut point_count = 0usize;
    let mut explicit_keys: Vec<Option<f64>> = Vec::new();
    let mut confidence_keys: Vec<Option<f64>> = Vec::new();
    let mut acc_values: Vec<Vec<Option<f64>>> = vec![Vec::new(); acc_cols.len()];
    let mut geoms_buf: Vec<Option<Geometry<f64>>> = Vec::new();
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

    for batch in reader {
        let batch = batch?;
        let gcol_idx = proj(geom_idx);
        let schema = batch.schema();
        let gfield = schema.field(gcol_idx);
        let garr = from_arrow_array(batch.column(gcol_idx).as_ref(), gfield)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
        geoms_buf.clear();
        extract_geometries_opt_from_array(garr.as_ref(), &mut geoms_buf)?;

        // `AssignFeature::index` is the GLOBAL ROW index: pass 2 addresses the
        // winner tables by raw row position. Rows with a null, empty, or
        // non-finite geometry produce no feature but still advance the row
        // index, so every row-keyed table stays aligned (H4 hardening; a
        // skipped row must never shift attributes onto a neighbor's geometry).
        let base = num_rows;
        for (i, gopt) in geoms_buf.iter().enumerate() {
            let Some(g) = gopt.as_ref().filter(|g| usable_geometry(g)) else {
                skipped_rows += 1;
                continue;
            };
            // Regional extract (#102): a feature whose bbox misses the region
            // produces no AssignFeature — its winner-table slot stays at the
            // UNASSIGNED sentinel, so pass 2 drops the row too. The row index
            // still advances (row-keyed tables stay aligned).
            let fbbox = geometry_bbox(g);
            if let Some(bb) = bbox_units {
                if !super::convert::bboxes_intersect(&fbbox, bb) {
                    continue;
                }
            }
            let kind = feature_kind(g);
            if matches!(kind, FeatureKind::Point) {
                point_count += 1;
            }
            if collect_lines && matches!(kind, FeatureKind::Line) {
                line_rows.push(base + i);
                line_feat_pos.push(features.len());
                line_geoms.push(g.clone());
            }
            features.push(AssignFeature {
                index: base + i,
                bbox: fbbox,
                kind,
                sort_key: None, // filled below once the ranking tier resolves
            });
        }
        num_rows += geoms_buf.len();

        match &mut plan {
            RankPlan::ExplicitSort { idx, .. } => {
                explicit_keys.extend(extract_sort_keys(batch.column(proj(*idx)).as_ref()));
            }
            RankPlan::ExplicitClass { idx, ranking } => {
                let col = batch.column(proj(*idx));
                explicit_keys.extend(extract_class_ranks(col.as_ref(), ranking)?);
                if collect_lines {
                    explicit_interner.extend(col.as_ref(), &mut explicit_groups);
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
        for (s, &idx) in acc_cols.iter().enumerate() {
            acc_values[s].extend(extract_sort_keys(batch.column(proj(idx)).as_ref()));
        }
    }

    let n = features.len();
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
    type Resolved = (
        Option<Vec<Option<f64>>>,
        RankingProvenance,
        Option<Vec<u32>>,
    );
    let (keys, provenance, all_groups): Resolved = match plan {
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
    };

    if let Some(keys) = keys {
        // Keys are extracted per ROW (including skipped-geometry rows), so
        // they are looked up by each feature's row index, not zipped
        // positionally.
        debug_assert_eq!(keys.len(), num_rows);
        for f in features.iter_mut() {
            f.sort_key = keys[f.index];
        }
    }

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

    Ok(Pass1Output {
        features,
        provenance,
        acc_values,
        coalesce,
        num_rows,
        skipped_rows,
    })
}

// ============================================================================
// Pass 2: per-level streaming filter → simplify → write
// ============================================================================

/// Wall-time accumulators for pass-2 stages ([profile] logging), stored as
/// nanoseconds. Atomic so the pipelined engine ([`super::pipeline`]) can share
/// one set across the parallel per-level processing of a batch; the serial
/// [`write_level_streaming`] path uses it single-threaded.
#[derive(Default)]
pub(super) struct Pass2Timers {
    /// Parquet read + Arrow decode of the raw batch (`reader.next()`).
    read: AtomicU64,
    /// Winner selection + geometry take/decode to `geo::Geometry`.
    decode: AtomicU64,
    /// Simplification (or verbatim vertex counting at the canonical level).
    simplify: AtomicU64,
    /// Output batch assembly (`build_level_batch`).
    build: AtomicU64,
}

impl Pass2Timers {
    fn add(cell: &AtomicU64, start: Instant) {
        cell.fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }
    /// Add a pre-measured duration (used by the reader thread for read time).
    pub(super) fn add_dur(cell: &AtomicU64, dur: Duration) {
        cell.fetch_add(dur.as_nanos() as u64, Ordering::Relaxed);
    }
    fn secs(cell: &AtomicU64) -> f64 {
        Duration::from_nanos(cell.load(Ordering::Relaxed)).as_secs_f64()
    }
    pub(super) fn read_cell(&self) -> &AtomicU64 {
        &self.read
    }
    /// Emit the aggregated per-stage breakdown ([profile] logging) for the
    /// pipelined engine, where stages interleave across levels so a per-level
    /// split is not meaningful.
    pub(super) fn log_engine_summary(&self, total_secs: f64, rows: usize) {
        let read_s = Self::secs(&self.read);
        let decode_s = Self::secs(&self.decode);
        let simplify_s = Self::secs(&self.simplify);
        let build_s = Self::secs(&self.build);
        log::debug!(
            "[profile] pass2 engine ({rows} rows): wall={total_secs:.2}s \
             read={read_s:.2}s decode={decode_s:.2}s simplify={simplify_s:.2}s \
             build={build_s:.2}s (stage sums are core-seconds, overlap wall)"
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
    /// Cascading simplification (#218): fine→coarse GSD chain ending at this
    /// level (`[gsd_finest-1, …, gsd_this]`), fed to
    /// [`simplify_cascade`]. Empty when cascading does not apply (cascade
    /// off, partitioning, or verbatim level) — the level then simplifies
    /// canonical geometry directly with `gsd_m`.
    cascade_chain: &'a [f64],
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
/// skipped, #211) plus `(rows_written, vertex_count)`.
#[allow(clippy::too_many_arguments)]
fn write_level_streaming(
    writer: &mut OverviewWriter<File>,
    level_idx: usize,
    hint: usize,
    source: &ConvertSource,
    read_batch_size: usize,
    in_flight: usize,
    row_groups: Option<&RowGroupSelection>,
    ctx: &LevelStreamCtx<'_>,
) -> Result<(LevelWriteOutcome, usize, usize), ConvertError> {
    let rows = Cell::new(0usize);
    let vertices = Cell::new(0usize);
    let timers = Pass2Timers::default();
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
    let (tx, rx) = bounded::<Processed>(in_flight.max(1));
    // Shared by reference into the producer thread (a `&Pass2Timers` is `Copy`,
    // so the `move` closure copies the borrow and leaves `timers` owned here
    // for the post-scope read).
    let timers = &timers;
    let outcome = std::thread::scope(|scope| -> Result<LevelWriteOutcome, ConvertError> {
        // Producer: read + process, in order, until EOF or the writer
        // hangs up. Returns the first stream/processing error, if any.
        // `move` transfers ownership of `tx` into the thread so it is dropped
        // when the producer finishes — that disconnect is what fuses the
        // writer's `rx.recv()` loop below (otherwise `recv` blocks forever and
        // deadlocks). Everything else the closure touches (`source`, `ctx`,
        // `&timers`, `row_groups`, the `usize`s) is `Copy`, so the outer
        // bindings — notably `timers`, read back after the scope — stay valid.
        let producer = scope.spawn(move || -> Result<(), ConvertError> {
            // Regional extract (#102): read the same per-part bbox-selected
            // row groups as pass 1, so the winner tables' global row indices
            // line up.
            let mut reader = source.open_stream(&ReadPlan {
                batch_size: read_batch_size.max(1),
                projection: None,
                row_groups,
            })?;
            let mut row_offset = 0usize;
            // Heartbeat (#242): the finest level re-streams the whole
            // input; keep the operator informed on planet-scale files
            // (quiet on small ones).
            let mut last_progress = Instant::now();
            loop {
                if last_progress.elapsed().as_secs() >= 10 {
                    last_progress = Instant::now();
                    log::info!(
                        "[convert] level {level_idx}: {row_offset} input \
                             row(s) scanned",
                    );
                }
                let t_read = Instant::now();
                let batch = match reader.next() {
                    None => return Ok(()),
                    Some(Err(e)) => return Err(e.into()),
                    Some(Ok(b)) => b,
                };
                Pass2Timers::add(&timers.read, t_read);
                let offset = row_offset;
                row_offset += batch.num_rows();
                match process_level_batch(&batch, offset, ctx, timers)? {
                    None => continue, // no members of this level in the batch
                    Some((out, verts)) => {
                        // Writer gone (its side errored and dropped `rx`):
                        // stop; the writer's error is reported below.
                        if tx.send(Processed { batch: out, verts }).is_err() {
                            return Ok(());
                        }
                    }
                }
            }
        });

        // Writer (this thread): drain processed batches in order. Dropping
        // the producer's `tx` (EOF, error, or writer-gone) fuses `recv`.
        let batches = std::iter::from_fn(|| match rx.recv() {
            Ok(msg) => {
                rows.set(rows.get() + msg.batch.num_rows());
                vertices.set(vertices.get() + msg.verts);
                Some(msg.batch)
            }
            Err(_) => None,
        });
        let res = writer.write_level(level_idx, Some(hint), batches);

        // A producer error takes precedence over the writer's (the writer
        // may merely observe a truncated stream). `join` cannot panic here:
        // the closure only returns `Result`.
        producer
            .join()
            .expect("finest-level producer thread panicked")?;
        Ok(res?)
    })?;
    let total = t_level.elapsed().as_secs_f64();
    let read_s = Pass2Timers::secs(&timers.read);
    let decode_s = Pass2Timers::secs(&timers.decode);
    let simplify_s = Pass2Timers::secs(&timers.simplify);
    let build_s = Pass2Timers::secs(&timers.build);
    // Read/decode/simplify/build run on the producer thread and overlap the
    // writer (#264), so these stage sums are core-seconds that overlap the
    // `total` wall time — the writer's own cost is roughly
    // `total - max(producer stages)`, not `total - sum`.
    log::debug!(
        "[profile] level {} ({}, {} rows): total={:.2}s read={:.2}s decode={:.2}s \
         simplify={:.2}s build={:.2}s (read/decode/simplify/build overlap the writer)",
        level_idx,
        if ctx.verbatim { "verbatim" } else { "simplify" },
        rows.get(),
        total,
        read_s,
        decode_s,
        simplify_s,
        build_s,
    );
    let fallbacks = full_resolution_fallback_count() - fallbacks_before;
    if fallbacks > 0 {
        log::debug!(
            "[profile] level {level_idx}: {fallbacks} feature(s) kept at full \
             resolution (invalid RDP candidate after all epsilon retries)"
        );
    }
    Ok((outcome, rows.get(), vertices.get()))
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
            let ml = ctx.min_levels[g];
            if ctx.duplicating {
                ml <= ctx.orig_level
            } else {
                ml == ctx.orig_level
            }
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
                } else if !ctx.cascade_chain.is_empty() {
                    simplify_cascade(g, ctx.cascade_chain, ctx.crs, ctx.simplify)
                } else {
                    simplify_for_level(g, ctx.gsd_m, ctx.crs, ctx.simplify)
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
fn assemble_level_batch(
    batch: &RecordBatch,
    row_offset: usize,
    ctx: &LevelStreamCtx<'_>,
    kept_idx: &[usize],
    kept_geoms: &[Geometry<f64>],
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
    // The incremental fold steps ctx-by-ctx; each level's cascade_chain must
    // be exactly the GSD suffix from the finest buffered level down to it,
    // or Serial and Pipelined would diverge.
    debug_assert!(ctxs
        .iter()
        .enumerate()
        .all(|(li, c)| c.cascade_chain.len() == ctxs.len() - li
            && c.cascade_chain.last() == Some(&c.gsd_m)));
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
        if finest.min_levels[g] <= finest.orig_level {
            *pos = u32::try_from(selected.len()).expect("batch rows fit in u32");
            selected.push(i);
        }
    }

    // Decode only the selected rows' geometries, once for all levels.
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(selected.len());
    if !selected.is_empty() {
        let take_idx = UInt32Array::from(selected.iter().map(|&i| i as u32).collect::<Vec<_>>());
        let geom_taken = take(batch.column(finest.geom_idx).as_ref(), &take_idx, None)?;
        let schema = batch.schema();
        let gfield = schema.field(finest.geom_idx);
        let garr = from_arrow_array(geom_taken.as_ref(), gfield)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
        extract_geometries_from_array(garr.as_ref(), &mut geoms)?;
    }
    Pass2Timers::add(&timers.decode, t_decode);

    // --- Per-feature incremental fold, fine→coarse, parallel over features.
    // folds[pos][d] is the result at ctxs[len-1-d]; entries stop at the
    // feature's coarsest member level, or earlier on Dropped (drops are
    // monotone fine→coarse, so a missing depth reads as dropped).
    let t_simplify = Instant::now();
    let folds: Vec<Vec<Simplified>> = geoms
        .par_iter()
        .zip(&selected)
        .map(|(g, &i)| {
            let ml = finest.min_levels[row_offset + i];
            let mut out: Vec<Simplified> = Vec::with_capacity(ctxs.len());
            let mut current: Option<Geometry<f64>> = None;
            for ctx in ctxs.iter().rev() {
                if ml > ctx.orig_level {
                    break; // duplicating membership is a contiguous fine suffix
                }
                let input = current.as_ref().unwrap_or(g);
                match simplify_for_level(input, ctx.gsd_m, ctx.crs, ctx.simplify) {
                    Simplified::Keep(s) => {
                        out.push(Simplified::Keep(s.clone()));
                        current = Some(s);
                    }
                    Simplified::Dropped => {
                        out.push(Simplified::Dropped);
                        break;
                    }
                }
            }
            out
        })
        .collect();
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
            let mut kept_geoms: Vec<Geometry<f64>> = Vec::new();
            let mut verts = 0usize;
            for (i, &pos) in pos_of_row.iter().enumerate() {
                let g = row_offset + i;
                if let Some(table) = ctx.coalesce_table {
                    if ctx.kinds.expect("kinds present when coalescing")[g] == FeatureKind::Line {
                        if let Some((merged, _)) = table.get(&g) {
                            verts += count_vertices(merged);
                            kept_idx.push(i);
                            kept_geoms.push(merged.clone());
                        }
                        continue;
                    }
                }
                if ctx.min_levels[g] <= ctx.orig_level {
                    debug_assert_ne!(pos, u32::MAX, "member row missing from cascade superset");
                    if let Some(Simplified::Keep(s)) = folds[pos as usize].get(depth) {
                        verts += count_vertices(s);
                        kept_idx.push(i);
                        kept_geoms.push(s.clone());
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
