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
//! a level degenerates during simplification — impossible under the default
//! knobs (the assign visibility gates, 2–4 × GSD, are stricter than the
//! simplify drop gate at 1 × GSD) and pathological otherwise; if it ever
//! happens the writer reports [`WriterError::EmptyLevel`] instead of silently
//! renumbering.
//!
//! [`WriterError::EmptyLevel`]: super::writer::WriterError::EmptyLevel

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use arrow_array::{Array, RecordBatch, UInt32Array};
use arrow_schema::{DataType, Schema, SchemaRef};
use arrow_select::take::take;
use geo::Geometry;
use geoarrow::array::from_arrow_array;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ProjectionMask;
use rayon::prelude::*;

use crate::batch_processor::extract_geometries_from_array;

use super::assign::{apply_density_budget, assign_levels, AssignFeature, FeatureKind};
use super::convert::{
    build_generalization, build_level_batch, build_source_schema, class_ranking_provenance,
    count_vertices, detect_crs, extract_class_ranks, extract_sort_keys, feature_kind,
    fill_level_bytes, find_geometry_column, geometry_bbox, mixed_geometry_field,
    overture_road_ranking, ClassRanking, ConvertError, ConvertOptions, ConvertReport, LevelReport,
    KNOWN_ROAD_CLASSES, ROAD_VOCAB_MIN_DISTINCT,
};
use super::level::{Crs, Mode, RankingProvenance};
use super::simplify::{
    full_resolution_fallback_count, simplify_for_level, Simplified, SimplifyOptions,
};
use super::writer::{LevelSpec, OverviewWriter, OverviewWriterOptions, LEVEL_COLUMN};

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

/// Streaming counterpart of [`super::convert::convert_to_overviews`].
pub(super) fn convert_streaming(
    input_path: &Path,
    output_path: &Path,
    options: &ConvertOptions,
) -> Result<ConvertReport, ConvertError> {
    let start = Instant::now();

    if options.sort_key.is_some() && options.class_ranking.is_some() {
        return Err(ConvertError::RankingConflict);
    }

    // CRS detection + rejection (spec Q3) — footer-only read.
    let crs = detect_crs(input_path)?;

    // Schema checks (level column, geometry column) — footer-only read.
    let file = File::open(input_path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let input_schema: SchemaRef = builder.schema().clone();
    drop(builder);

    if input_schema
        .fields()
        .iter()
        .any(|f| f.name().eq_ignore_ascii_case(LEVEL_COLUMN))
    {
        return Err(ConvertError::LevelColumnPresent);
    }
    let geom_idx = find_geometry_column(&input_schema).ok_or(ConvertError::NoGeometryColumn)?;
    let geom_field = input_schema.field(geom_idx).clone();

    // --- Pass 1: stream → AssignFeatures + resolved ranking. -----------------
    let t_pass1 = Instant::now();
    let (mut features, ranking_provenance) =
        run_pass1(input_path, &input_schema, geom_idx, options)?;
    let num_features = features.len();
    log::debug!(
        "[profile] pass1 stream+scan: {:.2}s",
        t_pass1.elapsed().as_secs_f64()
    );

    // --- Winner tables (assignment + Q2 density budget). ---------------------
    let level_specs = options.levels.resolve(options.gsd_base)?;
    let level_gsds: Vec<f64> = level_specs.iter().map(|(g, _)| *g).collect();

    let t_assign = Instant::now();
    let assignment = assign_levels(&features, &level_gsds, &options.assign, crs);
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
        "[profile] assignment+budget: {:.2}s",
        t_assign.elapsed().as_secs_f64()
    );

    // The winner table: per input row, the coarsest level it appears at.
    // 1 byte per feature — the only O(N) state carried into pass 2.
    let min_levels: Vec<u8> = assignment.assignments.iter().map(|a| a.min_level).collect();
    drop(assignment);
    features.clear();
    features.shrink_to_fit(); // free the pass-1 O(N)·48B scratch before pass 2

    let num_levels = level_gsds.len();
    let finest = num_levels.saturating_sub(1);

    // Per-level winner counts (exact row counts in partitioning mode; in
    // duplicating mode exact up to simplification drops — used as the writer's
    // row-group sizing hint and for empty-level omission).
    let mut hist = vec![0usize; num_levels];
    for &ml in &min_levels {
        hist[(ml as usize).min(finest)] += 1;
    }
    let counts: Vec<usize> = match options.mode {
        Mode::Duplicating => hist
            .iter()
            .scan(0usize, |acc, &c| {
                *acc += c;
                Some(*acc)
            })
            .collect(),
        Mode::Partitioning => hist,
    };

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

    // --- Writer setup (identical to the in-memory path). ---------------------
    let geom_name = geom_field.name().clone();
    let geom_out_field = mixed_geometry_field(&geom_name);
    let source_schema = build_source_schema(&input_schema, geom_idx, geom_out_field);

    let writer_levels: Vec<LevelSpec> = emitted
        .iter()
        .map(|e| LevelSpec::new(e.gsd, e.zoom))
        .collect();
    let emitted_gsds: Vec<f64> = emitted.iter().map(|e| e.gsd).collect();
    let mut writer_opts = OverviewWriterOptions::new(options.mode, writer_levels);
    writer_opts.max_row_group_size = options.max_row_group_size;
    writer_opts.full_column_stats = options.full_column_stats;
    writer_opts.cogp_compat_key = options.cogp_compat_key;
    writer_opts.generalization = Some(build_generalization(
        &emitted_gsds,
        crs,
        options,
        ranking_provenance,
    ));

    let mut writer = OverviewWriter::create(output_path, &source_schema, writer_opts)?;

    let non_geom_cols: Vec<usize> = (0..input_schema.fields().len())
        .filter(|&c| c != geom_idx)
        .collect();

    // --- Pass 2: per level, stream → filter → simplify → write. --------------
    let t_pass2 = Instant::now();
    let mut level_reports = Vec::with_capacity(emitted.len());
    for (level_idx, e) in emitted.iter().enumerate() {
        // Verbatim path: partitioning at every level (§2.3), and duplicating at
        // the canonical (finest) level (§2.4).
        let verbatim = matches!(options.mode, Mode::Partitioning) || e.orig as usize == finest;
        let ctx = LevelStreamCtx {
            source_schema: &source_schema,
            non_geom_cols: &non_geom_cols,
            geom_idx,
            min_levels: &min_levels,
            orig_level: e.orig,
            duplicating: matches!(options.mode, Mode::Duplicating),
            verbatim,
            gsd_m: e.gsd,
            crs,
            simplify: &options.simplify,
        };
        let (rows, vertices) = write_level_streaming(
            &mut writer,
            level_idx,
            e.hint,
            input_path,
            options.read_batch_size,
            &ctx,
        )?;
        level_reports.push(LevelReport {
            level: level_idx,
            gsd: e.gsd,
            zoom: e.zoom,
            feature_count: rows,
            vertex_count: vertices,
            uncompressed_bytes: 0,
            compressed_bytes: 0,
        });
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
        input_features: num_features,
        total_rows,
        total_vertices,
        total_compressed_bytes,
        duration_secs: start.elapsed().as_secs_f64(),
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

/// Pass 1: stream the input (geometry + ranking columns only) and produce the
/// per-feature [`AssignFeature`]s (with resolved sort keys) plus the ranking
/// provenance block (§3.5). Memory: `O(read batch)` transient +
/// `O(N)` small per-feature records.
fn run_pass1(
    input_path: &Path,
    input_schema: &Schema,
    geom_idx: usize,
    options: &ConvertOptions,
) -> Result<(Vec<AssignFeature>, RankingProvenance), ConvertError> {
    let mut plan = build_rank_plan(input_schema, options)?;

    // Project only the columns pass 1 needs: geometry + ranking candidates.
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
    cols.sort_unstable();
    cols.dedup();
    // Original schema index → projected batch column index.
    let proj = |orig: usize| cols.binary_search(&orig).expect("projected column");

    let file = File::open(input_path)?;
    let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
    let mask = ProjectionMask::roots(builder.parquet_schema(), cols.iter().copied());
    let reader = builder
        .with_projection(mask)
        .with_batch_size(options.read_batch_size.max(1))
        .build()?;

    let mut features: Vec<AssignFeature> = Vec::new();
    let mut point_count = 0usize;
    let mut explicit_keys: Vec<Option<f64>> = Vec::new();
    let mut confidence_keys: Vec<Option<f64>> = Vec::new();
    let mut geoms_buf: Vec<Geometry<f64>> = Vec::new();

    for batch in reader {
        let batch = batch?;
        let gcol_idx = proj(geom_idx);
        let schema = batch.schema();
        let gfield = schema.field(gcol_idx);
        let garr = from_arrow_array(batch.column(gcol_idx).as_ref(), gfield)
            .map_err(|e| crate::Error::GeoParquetRead(format!("geometry decode: {e}")))?;
        geoms_buf.clear();
        extract_geometries_from_array(garr.as_ref(), &mut geoms_buf)?;

        let base = features.len();
        for (i, g) in geoms_buf.iter().enumerate() {
            let kind = feature_kind(g);
            if matches!(kind, FeatureKind::Point) {
                point_count += 1;
            }
            features.push(AssignFeature {
                index: base + i,
                bbox: geometry_bbox(g),
                kind,
                sort_key: None, // filled below once the ranking tier resolves
            });
        }

        match &mut plan {
            RankPlan::ExplicitSort { idx, .. } => {
                explicit_keys.extend(extract_sort_keys(batch.column(proj(*idx)).as_ref()));
            }
            RankPlan::ExplicitClass { idx, ranking } => {
                explicit_keys.extend(extract_class_ranks(
                    batch.column(proj(*idx)).as_ref(),
                    ranking,
                )?);
            }
            RankPlan::Auto { roads, confidence } => {
                for cand in roads.iter_mut() {
                    let col = batch.column(proj(cand.idx));
                    scan_road_vocab(col.as_ref(), &mut cand.found);
                    cand.keys
                        .extend(extract_class_ranks(col.as_ref(), &cand.ranking)?);
                }
                if let Some((idx, _)) = confidence {
                    confidence_keys.extend(extract_sort_keys(batch.column(proj(*idx)).as_ref()));
                }
            }
            RankPlan::SizeFallback => {}
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

    // Resolve the tier (same order + logging as the in-memory path).
    let (keys, provenance): (Option<Vec<Option<f64>>>, RankingProvenance) = match plan {
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
                (Some(cand.keys), prov)
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
                )
            } else {
                (None, size_fallback())
            }
        }
        RankPlan::SizeFallback => (None, size_fallback()),
    };

    if let Some(keys) = keys {
        debug_assert_eq!(keys.len(), features.len());
        for (f, k) in features.iter_mut().zip(keys) {
            f.sort_key = k;
        }
    }

    Ok((features, provenance))
}

// ============================================================================
// Pass 2: per-level streaming filter → simplify → write
// ============================================================================

/// Wall-time accumulators for one level's pass-2 stream ([profile] logging).
#[derive(Default)]
struct Pass2Timers {
    /// Parquet read + Arrow decode of the raw batch (`reader.next()`).
    read: Cell<Duration>,
    /// Winner selection + geometry take/decode to `geo::Geometry`.
    decode: Cell<Duration>,
    /// Simplification (or verbatim vertex counting at the canonical level).
    simplify: Cell<Duration>,
    /// Output batch assembly (`build_level_batch`).
    build: Cell<Duration>,
}

impl Pass2Timers {
    fn add(cell: &Cell<Duration>, start: Instant) {
        cell.set(cell.get() + start.elapsed());
    }
}

/// Immutable context for one level's pass-2 stream.
struct LevelStreamCtx<'a> {
    source_schema: &'a Schema,
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
}

/// Stream one level from the input file into the writer. Returns
/// `(rows_written, vertex_count)`.
fn write_level_streaming(
    writer: &mut OverviewWriter<File>,
    level_idx: usize,
    hint: usize,
    input_path: &Path,
    read_batch_size: usize,
    ctx: &LevelStreamCtx<'_>,
) -> Result<(usize, usize), ConvertError> {
    let file = File::open(input_path)?;
    let mut reader = ParquetRecordBatchReaderBuilder::try_new(file)?
        .with_batch_size(read_batch_size.max(1))
        .build()?;

    // `write_level` consumes an infallible batch iterator; errors inside the
    // stream are parked in `err` (fusing the iterator) and re-raised after.
    let err: RefCell<Option<ConvertError>> = RefCell::new(None);
    let rows = Cell::new(0usize);
    let vertices = Cell::new(0usize);
    let mut row_offset = 0usize;
    let timers = Pass2Timers::default();
    let fallbacks_before = full_resolution_fallback_count();
    let t_level = Instant::now();

    let batches = std::iter::from_fn(|| loop {
        let t_read = Instant::now();
        let batch = match reader.next() {
            None => return None,
            Some(Err(e)) => {
                *err.borrow_mut() = Some(e.into());
                return None;
            }
            Some(Ok(b)) => b,
        };
        Pass2Timers::add(&timers.read, t_read);
        let offset = row_offset;
        row_offset += batch.num_rows();
        match process_level_batch(&batch, offset, ctx, &timers) {
            Ok(None) => continue, // no members of this level in the batch
            Ok(Some((out, verts))) => {
                rows.set(rows.get() + out.num_rows());
                vertices.set(vertices.get() + verts);
                return Some(out);
            }
            Err(e) => {
                *err.borrow_mut() = Some(e);
                return None;
            }
        }
    });

    let res = writer.write_level(level_idx, Some(hint), batches);
    if let Some(e) = err.borrow_mut().take() {
        return Err(e); // stream error takes precedence over the writer's
    }
    res?;
    let total = t_level.elapsed();
    let accounted =
        timers.read.get() + timers.decode.get() + timers.simplify.get() + timers.build.get();
    log::debug!(
        "[profile] level {} ({}, {} rows): total={:.2}s read={:.2}s decode={:.2}s \
         simplify={:.2}s build={:.2}s write={:.2}s",
        level_idx,
        if ctx.verbatim { "verbatim" } else { "simplify" },
        rows.get(),
        total.as_secs_f64(),
        timers.read.get().as_secs_f64(),
        timers.decode.get().as_secs_f64(),
        timers.simplify.get().as_secs_f64(),
        timers.build.get().as_secs_f64(),
        total.saturating_sub(accounted).as_secs_f64(),
    );
    let fallbacks = full_resolution_fallback_count() - fallbacks_before;
    if fallbacks > 0 {
        log::debug!(
            "[profile] level {level_idx}: {fallbacks} feature(s) kept at full \
             resolution (invalid RDP candidate after all epsilon retries)"
        );
    }
    Ok((rows.get(), vertices.get()))
}

/// Process one input batch for one level: select the level's members from the
/// winner table, decode only their geometries, simplify (unless verbatim), and
/// assemble the output batch. Returns `None` when no member row survives.
fn process_level_batch(
    batch: &RecordBatch,
    row_offset: usize,
    ctx: &LevelStreamCtx<'_>,
    timers: &Pass2Timers,
) -> Result<Option<(RecordBatch, usize)>, ConvertError> {
    let n = batch.num_rows();
    let t_decode = Instant::now();
    let selected: Vec<usize> = (0..n)
        .filter(|&i| {
            let ml = ctx.min_levels[row_offset + i];
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
        let simplified: Vec<Simplified> = geoms
            .par_iter()
            .map(|g| simplify_for_level(g, ctx.gsd_m, ctx.crs, ctx.simplify))
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
    let out_batch = build_level_batch(
        ctx.source_schema,
        batch,
        ctx.non_geom_cols,
        ctx.geom_idx,
        &kept_idx,
        &kept_geoms,
    )?;
    Pass2Timers::add(&timers.build, t_build);
    Ok(Some((out_batch, verts)))
}
