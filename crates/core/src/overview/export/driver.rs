use super::*;

/// Split the level plans into consecutive groups that export concurrently
/// (#647).
///
/// Coarse levels hold one partition and a few hundred tiles at most, so a
/// strictly serial level loop leaves most cores idle until each one
/// finishes. A group encodes its levels concurrently and writes them in
/// level order (see [`export_level_group`]). Every tile id at zoom `z` is
/// below every id at `z + 1`, so the writer still sees ascending ids and the
/// archive is byte-identical.
///
/// A level joins the open group if it fits in a single wave (empty levels
/// count, with zero members) and its own members fit `member_cap`. With
/// `overlap` that is all: the group's in-flight gate ([`group_schedule`]),
/// not its sum, bounds the members held, so the finest level's collect
/// overlaps the coarser levels' last encodes instead of waiting for the
/// group to end. Without it (`bounded`) the group's summed members also stay
/// within `member_cap`, so bounded's peak does not rise. A multi-wave level,
/// or a level that alone exceeds the cap, is a group of one and exports
/// through the unchanged wave loop. A `member_cap` of 0 is the serial
/// schedule, one level per group.
pub(super) fn plan_level_groups(
    plans: &[LevelPlan],
    member_cap: usize,
    (waves, overlap): (bool, bool),
) -> Vec<std::ops::Range<usize>> {
    if member_cap == 0 {
        return (0..plans.len()).map(|i| i..i + 1).collect();
    }
    let mut groups: Vec<std::ops::Range<usize>> = Vec::new();
    let mut open: Option<(std::ops::Range<usize>, usize)> = None;
    for (i, plan) in plans.iter().enumerate() {
        let members: usize = plan.partitions.iter().map(|p| p.members).sum();
        let single_wave = plan.wave_count() <= 1;
        if waves && !single_wave {
            // #817: its waves are bounded in flight and in write order, not
            // by the group's sum, so it joins the open group.
            match open.as_mut() {
                Some((range, _)) => range.end = i + 1,
                None => open = Some((i..i + 1, 0)),
            }
            continue;
        }
        if let Some((range, sum)) = open.as_mut() {
            let fits =
                members <= member_cap && (overlap || sum.saturating_add(members) <= member_cap);
            if single_wave && fits {
                range.end = i + 1;
                *sum += members;
                continue;
            }
        }
        if let Some((range, _)) = open.take() {
            groups.push(range);
        }
        if single_wave && members <= member_cap {
            open = Some((i..i + 1, members));
        } else {
            groups.push(i..i + 1);
        }
    }
    groups.extend(open.map(|(range, _)| range));
    groups
}

/// The level groups `plans` export in ([`plan_level_groups`]) and how each
/// runs. Its members in flight are capped at `member_cap`, bounded by the
/// largest wave of any level but never below [`PARTITION_WAVE_MIN`]
/// partitions of `partition_target` (the narrowest full wave the serial loop
/// holds), so a small export's levels all overlap. Collected members wait,
/// valve and MVT workspace included, until their level encodes, so members
/// in flight past the largest wave raise the export's peak above the serial
/// loop's, here by at most the slack below. The cap bounds members in
/// flight, not the group, so a group whose encodes keep up still overlaps
/// every level.
///
/// Under `auto` and `speed` the cap adds [`WAVE_SLACK_PERMILLE`] to the
/// largest wave, within `member_cap`: the next level is admitted once the
/// members before it have at most the slack left, so their last tiles no
/// longer idle the pool. With the finest level in the group
/// ([`plan_level_groups`]) its collect overlaps the coarser levels' last
/// encodes: 3–11% less levels wall for up to 4% (59–116 MiB) more peak
/// RSS on moldova, lines, wideprops and brmt.
///
/// A multi-wave level's waves overlap (#817). Under `bounded` a group that
/// holds one caps at the largest wave plus the slack, with no
/// [`PARTITION_WAVE_MIN`] floor, which its plan priced in (#815). A group of
/// single-wave levels keeps the floor and no slack, so a small export's
/// levels still overlap and bounded's peak does not rise. A `member_cap` of
/// 0 is the serial schedule.
pub(super) fn group_schedule(
    member_cap: usize,
    plans: &[LevelPlan],
    partition_target: usize,
    profile: MemoryProfile,
) -> (Vec<std::ops::Range<usize>>, GroupGate) {
    let largest_wave = plans
        .iter()
        .flat_map(|plan| {
            plan.waves()
                .map(|wave| wave.iter().map(|p| p.members).sum::<usize>())
        })
        .max()
        .unwrap_or(0);
    // Bounded keeps the serial loop's peak: no slack past its largest
    // single-wave level and no group past the member cap.
    let overlap = profile != MemoryProfile::Bounded;
    let floored = largest_wave.max(partition_target.saturating_mul(PARTITION_WAVE_MIN));
    let single_wave_cap = member_cap.min(if overlap {
        with_wave_slack(floored)
    } else {
        floored
    });
    // Bounded's multi-wave levels plan at a quarter target, so the floor
    // would let a guard-narrowed level's waves past what was priced.
    let cap = match profile {
        MemoryProfile::Bounded => member_cap.min(with_wave_slack(largest_wave)),
        _ => single_wave_cap,
    };
    let gate = GroupGate {
        cap,
        single_wave_cap,
        waves: cap > 0,
        overlap,
    };
    (
        plan_level_groups(plans, member_cap, (gate.waves, overlap)),
        gate,
    )
}

/// Snapshot a salvageable archive capped at `zoom` after a level group
/// (#229/#459's throttled checkpoint), so an interrupted run keeps its
/// finished zooms. A no-op when `zoom` is `None` (after the final group,
/// where `finalize` immediately follows) or when less than
/// [`CHECKPOINT_INTERVAL`] has passed since the last one. Pulled out of
/// `export_pmtiles_run` to keep it under clippy's function-length ceiling.
/// Resets the checkpoint throttle clock, and records the checkpoint's
/// duration and count in the profile.
pub(super) fn checkpoint_if_due(
    writer: &mut StreamingPmtilesWriter,
    ckpt: &CheckpointCtx<'_>,
    zoom: Option<u8>,
    progress: &mut ExportProgress,
) -> Result<(), ExportError> {
    let Some(zoom) = zoom else { return Ok(()) };
    if progress.checkpoint_throttle.elapsed() < progress.checkpoint_interval {
        return Ok(());
    }
    let t_ckpt = Instant::now();
    writer.checkpoint(ckpt.output_path)?;
    let dur = t_ckpt.elapsed();
    progress.checkpoint_throttle = Instant::now();
    add_nanos(&progress.profile.timers.checkpoint, dur);
    progress.profile.checkpoints += 1;
    #[cfg(test)]
    progress.checkpoint_zooms.push(zoom);
    log::info!(target: LOG_TARGET,
        "[export] checkpoint written: zooms {}..={zoom} salvageable at {} ({:.2}s)",
        ckpt.coarsest_zoom,
        writer.salvage_path().unwrap_or(ckpt.output_path).display(),
        dur.as_secs_f64(),
    );
    Ok(())
}

/// Where [`checkpoint_if_due`] writes, and the coarsest zoom it logs.
pub(super) struct CheckpointCtx<'a> {
    pub(super) output_path: &'a Path,
    pub(super) coarsest_zoom: u8,
}

/// The #235 single-read fill's member-store backing when the caller did not
/// force one: the profile's choice, by the rule convert's sinks use
/// ([`sink_backing`]): `speed` RAM, `bounded` spill, `auto` the
/// RAM-vs-spill estimate over every level's buffered members (#788).
pub(super) fn fill_backing(
    scans: &[LevelScan],
    num_levels: usize,
    profile: MemoryProfile,
    budget: &MemoryBudget,
    mean_member_bytes: Option<u64>,
) -> SinkBacking {
    let buffered_rows: usize = scans.iter().map(|s| s.feature_count).sum();
    let b = sink_backing(
        budget,
        profile,
        Mode::Partitioning,
        buffered_rows,
        None,
        mean_member_bytes,
        None,
    );
    log::info!(target: LOG_TARGET,
        "[export] pass2 single-read fan-out (#235): ~{buffered_rows} buffered \
         member row(s) across {num_levels} level(s), {} → {b:?}",
        profile.as_str(),
    );
    b
}

/// Export-wide mutable state threaded through the per-level loop as one
/// `&mut` (keeps `export_pmtiles_impl` under clippy's function-length
/// ceiling). Two owners, kept apart on purpose:
///
/// - `checkpoint_throttle` is BEHAVIOR: the #229 incremental-checkpoint
///   throttle clock, which decides when a salvage snapshot is written.
/// - `profile` is diagnostics only (#535): nothing in it is ever read by
///   anything that affects the archive.
pub(super) struct ExportProgress {
    pub(super) fail_level: Option<usize>,
    /// [`Pass2Knobs::panic_write_level`].
    #[cfg(test)]
    pub(super) panic_write_level: Option<usize>,
    /// When the last checkpoint was written (or the export started): a
    /// checkpoint is due once [`CHECKPOINT_INTERVAL`] has elapsed since. A
    /// fast export finishes before the first interval elapses and never
    /// checkpoints, letting `finalize` do all the work.
    pub(super) checkpoint_throttle: Instant,
    /// The throttle's interval: [`CHECKPOINT_INTERVAL`], or a test's value.
    pub(super) checkpoint_interval: Duration,
    /// The zoom each checkpoint was capped at, in order (#647 tests).
    #[cfg(test)]
    pub(super) checkpoint_zooms: Vec<u8>,
    pub(super) profile: ExportProfile,
    /// #571: the continuous background sampler, running only when
    /// `TYLERTOO_PROFILE_JSON` is set — catches an intra-phase peak (e.g. the
    /// partitioning-mode fill, or a single dense level's wave loop) that the
    /// `end_*` boundary samples below cannot see, the same gap #571 fixed on
    /// the convert side. `None`, no thread, when profiling is off.
    pub(super) rss_sampler: Option<RssSampler>,
    /// #810: the sampler's current phase and the stage core-seconds
    /// ([`ExportStageSecs::core_secs`]) at its start.
    pub(super) stage_mark: (&'static str, f64),
    /// #810: stage core-seconds per sampler phase, credited to the
    /// sampler's report when it finishes.
    pub(super) phase_stage_secs: Vec<(&'static str, f64)>,
}

impl ExportProgress {
    /// `timers` are made before the level scan, which charges its `scan_*`
    /// stages to them before this takes them over (#730).
    pub(super) fn new(
        num_levels: usize,
        timers: Arc<ExportTimers>,
        rss_sampler: Option<RssSampler>,
        knobs: &Pass2Knobs,
    ) -> Self {
        Self {
            checkpoint_throttle: Instant::now(),
            checkpoint_interval: knobs.checkpoint_interval,
            fail_level: knobs.fail_level,
            #[cfg(test)]
            panic_write_level: knobs.panic_write_level,
            #[cfg(test)]
            checkpoint_zooms: Vec::new(),
            profile: ExportProfile {
                timers,
                walls: ExportPhaseWalls::default(),
                per_zoom: Vec::with_capacity(num_levels),
                waves_total: 0,
                checkpoints: 0,
                peak_rss_mib: None,
                rss_sampler_report: None,
            },
            rss_sampler,
            stage_mark: ("scan", 0.0),
            phase_stage_secs: Vec::new(),
        }
    }

    /// Attribute every sample the background sampler takes from now on to
    /// `phase`, until the next call. A no-op when profiling is off.
    pub(super) fn enter_phase(&mut self, phase: &'static str) {
        crate::tile_trace::event("phase", || serde_json::json!({ "phase": phase }));
        if let Some(sampler) = &self.rss_sampler {
            sampler.mark_phase(phase);
            self.close_stage_phase();
            self.stage_mark.0 = phase;
        }
    }

    /// #810: charge the stage core-seconds since the current phase began to it.
    pub(super) fn close_stage_phase(&mut self) {
        let total = self.profile.timers.stage_secs().core_secs();
        let (phase, start) = self.stage_mark;
        self.phase_stage_secs.push((phase, total - start));
        self.stage_mark.1 = total;
    }

    /// Take a boundary RSS sample into `peak_rss_mib`, and hand it to the
    /// #571 sampler too (credited to the current phase), so the sampler's
    /// `true_peak_mib` is never below the boundary peak beside it.
    pub(super) fn sample_boundary_rss(&mut self) {
        let sample = sample_rss_peak(&mut self.profile.peak_rss_mib);
        if let (Some(sampler), Some(mib)) = (&self.rss_sampler, sample) {
            sampler.record(mib);
        }
    }

    /// Close the scan (+ plan) phase wall and sample RSS.
    pub(super) fn end_scan(&mut self, t_scan: Instant) {
        self.profile.walls.scan = t_scan.elapsed().as_secs_f64();
        self.sample_boundary_rss();
    }

    /// Close the partitioning-mode fill phase wall and sample RSS.
    pub(super) fn end_fill(&mut self, t_fill: Instant) {
        self.profile.walls.fill = t_fill.elapsed().as_secs_f64();
        self.sample_boundary_rss();
    }

    /// Close the finalize phase wall and sample RSS.
    pub(super) fn end_finalize(&mut self, t_finalize: Instant) {
        self.profile.walls.finalize = t_finalize.elapsed().as_secs_f64();
        self.sample_boundary_rss();
    }

    /// Stop the sampler (a no-op if profiling was off) and fold its report
    /// into the profile that `emit` writes.
    pub(super) fn finish_rss_sampler(&mut self) {
        self.close_stage_phase();
        self.profile.rss_sampler_report = self.rss_sampler.take().map(|sampler| {
            let mut report = sampler.finish();
            for &(phase, secs) in &self.phase_stage_secs {
                report.credit_stage_secs(phase, secs);
            }
            report
        });
    }
}

/// The export's `TYLERTOO_PROFILE_JSON` accumulators (#535): stage timers,
/// phase walls, and the per-zoom split. Diagnostics only.
pub(super) struct ExportProfile {
    pub(super) timers: Arc<ExportTimers>,
    pub(super) walls: ExportPhaseWalls,
    pub(super) per_zoom: Vec<ExportZoomProfile>,
    pub(super) waves_total: usize,
    pub(super) checkpoints: u64,
    pub(super) peak_rss_mib: Option<f64>,
    /// #571: the continuous sampler's findings, filled by
    /// [`ExportProgress::finish_rss_sampler`] just before `emit`. `None` only
    /// when profiling was off.
    pub(super) rss_sampler_report: Option<RssSamplerReport>,
}

/// The per-export facts [`ExportProfile::emit`] writes alongside the
/// accumulators.
pub(super) struct ExportProfileTarget<'a> {
    pub(super) output: &'a Path,
    pub(super) layer_name: &'a str,
    pub(super) mode: &'a str,
    pub(super) partition_wave_width: usize,
    /// Start of the whole export: `phase_walls.total` is elapsed from here.
    pub(super) start: Instant,
}

impl ExportProfile {
    /// Write the export's `TYLERTOO_PROFILE_JSON` line (a no-op when the var
    /// is unset). See [`write_export_profile_json`] for why this is a SECOND
    /// JSONL line rather than merged into convert's object.
    pub(super) fn emit(
        &self,
        target: &ExportProfileTarget<'_>,
        diagnostics: &crate::diagnostics::Diagnostics,
    ) {
        write_export_profile_json(
            ExportProfileJsonInputs {
                output: target.output,
                layer_name: target.layer_name,
                mode: target.mode,
                phase_walls: self.walls,
                total_secs: target.start.elapsed().as_secs_f64(),
                stage_secs: self.timers.stage_secs(),
                per_zoom: &self.per_zoom,
                waves_total: self.waves_total,
                partition_wave_width: target.partition_wave_width,
                checkpoints: self.checkpoints,
                peak_rss_mib: self.peak_rss_mib,
                rss_sampler_report: &self.rss_sampler_report,
            },
            diagnostics,
        );
    }
}

/// Which columns the tiles carry, under which names, and the `--feature-id`
/// column -- all settled before the output is created.
///
/// #359: the published names are derived from the file's own rename
/// provenance, so a standalone `export-pmtiles` on an overview written by an
/// earlier run restores names just as `tiles` does; #386: then the caller's
/// include/exclude, matched on the published names. #443: `--feature-id` is
/// resolved (withholding its column from the tile properties and
/// `vector_layers`) and every row of the file checked, so a bad id fails in
/// seconds with its true overview-file row, not after the coarser levels
/// export.
pub(super) fn resolve_export_columns(
    (reader, band): (&OverviewReader, BandRead<'_>),
    options: &ExportOptions,
    level_zooms: &[u8],
) -> Result<ResolvedColumns, ExportError> {
    let geom_idx =
        reader
            .resolved_schema()
            .geometry_index()
            .ok_or_else(|| ExportError::NoGeometryColumn {
                path: Some(reader.path().display().to_string()),
            })?;
    if let Some(by_name) = reader.resolved_schema().geometry_name_disagreement() {
        log::warn!(target: LOG_TARGET,
            "[export] tiling geometry column {:?}, the file's `primary_column`, not {:?}, \
             which its name suggests; an overview converted before #675 may declare the \
             wrong one when its input had several geometry columns: re-convert it if \
             the tiles show the wrong geometry",
            reader.schema().field(geom_idx).name(),
            by_name
        );
    }
    let mut published = PublishedNames::from_reader(reader).with_selection(
        reader.schema(),
        geom_idx,
        &options.properties,
        &options.feature_order,
    )?;
    // #434: said once per column, here, where the schema is resolved once
    // per export -- never from the per-batch extraction, which would repeat
    // it for every row group of every level.
    let skipped = skipped_property_columns(reader.schema(), geom_idx, &published);
    for col in &skipped {
        log::warn!(target: LOG_TARGET,
            "[export] not exporting property {:?}: its type {} has no MVT encoding \
             (strings, numbers, booleans, temporals and decimals are encoded directly; \
             struct/list/map columns as JSON strings); cast or encode it as text first, \
             e.g. with gpio or DuckDB",
            col.name,
            col.data_type
        );
    }
    let feature_id = resolve_feature_id(
        reader.schema(),
        geom_idx,
        reader.meta(),
        options,
        &mut published,
    )?;
    published.withhold_counter_at_canonical(reader.meta(), &options.properties);
    if let Some(fid) = &feature_id {
        validate_feature_id_column((reader, band), fid, level_zooms)?;
    }
    Ok(ResolvedColumns {
        published,
        feature_id,
        skipped,
    })
}

/// What [`resolve_export_columns`] settled once per export.
pub(super) struct ResolvedColumns {
    pub(super) published: PublishedNames,
    pub(super) feature_id: Option<ResolvedFeatureId>,
    /// The columns warned about and left out (#434), for the report.
    pub(super) skipped: Vec<SkippedPropertyColumn>,
}

/// Everything one level's pass-2 render reads besides the writer and the
/// member store.
pub(super) struct ExportLevelCtx<'a> {
    pub(super) reader: &'a OverviewReader,
    /// How the band is read: [`read_band`].
    pub(super) band: BandRead<'a>,
    /// This level's scan-pass result (feature count, bounds, tile counts).
    pub(super) scan: &'a LevelScan,
    /// This level's partitions and wave width, fixed by [`plan_levels`].
    pub(super) plan: &'a LevelPlan,
    pub(super) level_idx: usize,
    pub(super) num_levels: usize,
    pub(super) crs: Crs,
    pub(super) published: &'a PublishedNames,
    /// The resolved `--feature-id` column (#443), if any.
    pub(super) feature_id: Option<&'a ResolvedFeatureId>,
    pub(super) options: &'a ExportOptions,
    pub(super) gzip_cache: &'a Arc<TileGzipCache>,
    /// Start of the whole export, for the elapsed-time progress lines.
    pub(super) start: Instant,
    /// The #750 coverage fill, when asked for.
    pub(super) fills: Option<&'a coverage_fill::FillMap>,
    /// #803: the level's stopped tiles, in key order, written among its own.
    pub(super) terminals: &'a [EncodedTile],
    /// How a level of several waves reads its band when no export-wide
    /// member store is filled: once, into a one-level [`MemberStore`]
    /// (#740), or, `None`, once per wave.
    pub(super) level_read: Option<LevelRead>,
    /// Resident bytes of one member, the price a level group's cap is sized
    /// at ([`queued_members`]), when known.
    pub(super) member_bytes: Option<u64>,
    /// #802: the finest level's junction pins ([`finest_level_pins`]).
    pub(super) pins: Option<&'a JunctionNodes>,
    /// #768: the valve floors, with `--valve-scope smooth|zoom`.
    pub(super) floors: Option<&'a valve_predict::ValveFloors>,
    /// The export's test-only work counts (#788).
    #[cfg(test)]
    pub(super) work: &'a Mutex<ExportWorkTally>,
}

impl<'a> ExportLevelCtx<'a> {
    /// The per-wave context [`process_wave`] reads.
    pub(super) fn level_ctx(&self, timers: &'a ExportTimers) -> LevelCtx<'a> {
        LevelCtx {
            reader: self.reader,
            band: self.band,
            level_idx: self.level_idx,
            crs: self.crs,
            zoom: self.plan.zoom,
            opts: self.options,
            gzip_cache: self.gzip_cache,
            published: self.published,
            feature_id: self.feature_id,
            timers,
            fills: self.fills,
            tiles: self
                .options
                .variable_depth
                .then_some(&self.scan.tile_counts),
            pins: self.pins,
            floors: self.floors,
            #[cfg(test)]
            work: self.work,
        }
    }
}

/// Render and write one level's tiles, then report what it produced.
///
/// Partitions are processed in ascending tile order, and each finished
/// partition's encoded tiles stream straight to the writer. To hide the
/// per-partition band re-read/decode behind clip work, partitions run in small
/// parallel waves (order-preserving collect, then a serial in-order write), so
/// peak memory is O(one wave of partitions + writer state), not O(zoom band).
pub(super) fn export_level(
    writer: &mut StreamingPmtilesWriter,
    mut store: Option<&mut MemberStore>,
    level: &ExportLevelCtx<'_>,
    budget: &mut MemoryBudget,
    progress: &mut ExportProgress,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<ZoomReport, ExportError> {
    let ExportLevelCtx {
        plan,
        level_idx,
        num_levels,
        start,
        ..
    } = *level;
    let zoom = plan.zoom;
    let timers = &progress.profile.timers;

    let t_tiles = Instant::now();
    let t_level = crate::tile_trace::mark();
    let ctx = level.level_ctx(timers);
    let mut tally = LevelTally::default();
    let total_waves = plan.wave_count();
    // Within-level progress (#229): a long finest level is where runs get
    // stuck, so emit a throttled wave counter. If it advances the level is
    // slow; if it freezes the level is stuck — diagnosable in minutes.
    let mut last_wave_log = Instant::now();
    let (mut level_store, level_grants) = match level.level_read {
        Some(read) if store.is_none() && total_waves > 1 => {
            let t_fill = Instant::now();
            let (backing, grants) = level_read_backing(read, plan, budget, 0);
            let filled = fill_level_store(&ctx, plan, backing, diagnostics)?;
            trace_fill(t_fill, level, false);
            (Some(filled), grants)
        }
        _ => (None, Vec::new()),
    };
    tally.spill_bytes = match (&level_store, &store) {
        (Some(s), _) => s.level_spilled_bytes(0),
        (None, Some(s)) => s.level_spilled_bytes(level_idx),
        (None, None) => 0,
    };
    for (wave_idx, wave) in plan.waves().enumerate() {
        let results: Vec<Vec<EncodedTile>> = crate::tile_trace::job(
            || crate::tile_trace::Ctx::wave(level_idx, zoom, wave_idx),
            || match (level_store.as_mut(), store.as_deref_mut()) {
                (Some(s), _) => encode_wave_from_store(s, 0, &ctx, wave_idx, wave, diagnostics),
                (None, Some(s)) => {
                    encode_wave_from_store(s, level_idx, &ctx, wave_idx, wave, diagnostics)
                }
                (None, None) => process_wave(&ctx, wave, diagnostics),
            },
        )?;
        write_encoded_tiles(writer, zoom, &results, level.terminals, &mut tally, timers)?;

        if last_wave_log.elapsed() >= WAVE_LOG_INTERVAL {
            log::info!(target: LOG_TARGET,
                "[export] level {}/{num_levels} z{zoom}: wave {}/{total_waves}, \
                 {} tiles, {:.0}s",
                level_idx + 1,
                wave_idx + 1,
                tally.tile_count,
                start.elapsed().as_secs_f64(),
            );
            last_wave_log = Instant::now();
        }
    }
    write_terminals(writer, zoom, level.terminals, None, &mut tally, timers)?;
    for grant in level_grants {
        budget.release(grant);
    }
    crate::tile_trace::span("serial_level", t_level, || {
        let mut v = wave_fields(level, 0);
        v["waves"] = plan.wave_count().into();
        v["members"] = plan
            .partitions
            .iter()
            .map(|p| p.members)
            .sum::<usize>()
            .into();
        v
    });
    Ok(finish_level(
        level,
        &tally,
        total_waves,
        LevelWalls::serial(t_tiles.elapsed().as_secs_f64(), tally.write_secs),
        progress,
    ))
}

/// The #750 coverage fill for this export, when [`ExportOptions::coverage_fill`]
/// asks for it: built from the finest of `levels` for every coarser zoom,
/// before any level is written. Not for a data shard (a tile range), which
/// owns zooms at and past the pivot, where the fill matters least; a coarse
/// job (a zoom ceiling) builds it from the file's finest level, which it
/// reads but does not emit (#771 option 1).
pub(super) fn build_fills_if_asked(
    levels: &[ExportLevelCtx<'_>],
    budget: &mut MemoryBudget,
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Option<coverage_fill::FillMap>, ExportError> {
    let (Some(first), Some(last)) = (levels.first(), levels.last()) else {
        return Ok(None);
    };
    let options = first.options;
    if !options.coverage_fill {
        return Ok(None);
    }
    if options.tile_range.is_some() {
        log::warn!(target: LOG_TARGET,
            "[export] --coverage-fill is ignored with --tile-range: a data shard does not \
             build the fill; the coarse job (--zoom-ceiling) does"
        );
        return Ok(None);
    }
    if options.extent != coverage_fill::FILL_EXTENT as u32 {
        log::warn!(target: LOG_TARGET,
            "[export] --coverage-fill is ignored at extent {}: the fill is sized for \
             the default extent of 4096",
            options.extent
        );
        return Ok(None);
    }
    // #788: the builder's workspace, from the export-wide ledger, returned
    // once the map is built; the map itself is held per zoom from then on.
    let grant = budget.fit(
        options.profile,
        MemoryPhase::Fill,
        Price::new(1, PriceBasis::Measured),
        coverage_fill::WORKSPACE_WANT_BYTES,
        coverage_fill::WORKSPACE_FLOOR_BYTES,
    );
    let t_fill = crate::tile_trace::mark();
    let built =
        coverage_fill::build_fill_map(last, first.plan.zoom, grant.bytes(), timers, diagnostics);
    crate::tile_trace::span("fill", t_fill, || serde_json::json!({ "kind": "coverage" }));
    budget.release(grant);
    built.map(Some)
}

/// The memory the levels draw on: the export-wide member store (#235), when
/// one was filled, and the run's budget, which grants a multi-wave level's
/// own store (#740).
pub(super) struct LevelMemory<'s> {
    pub(super) store: Option<&'s mut MemberStore>,
    pub(super) budget: &'s mut MemoryBudget,
}

/// Export every level, one [`plan_level_groups`] group at a time. Each level
/// is followed by a throttled salvage checkpoint (#229) once its tiles are
/// written, inside a group too (#647).
pub(super) fn export_level_groups(
    writer: &mut StreamingPmtilesWriter,
    mem: LevelMemory<'_>,
    levels: &[ExportLevelCtx<'_>],
    (groups, gate): (&[std::ops::Range<usize>], GroupGate),
    ckpt: &CheckpointCtx<'_>,
    progress: &mut ExportProgress,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<ZoomReport>, ExportError> {
    let LevelMemory { mut store, budget } = mem;
    let mut fills = build_fills_if_asked(levels, budget, &progress.profile.timers, diagnostics)?;
    let mut zooms = Vec::with_capacity(levels.len());
    for group in groups {
        // #788: each level owns its zoom's fill, taken out of the map as
        // its group starts and dropped when the group is written.
        let group_fills: Vec<Option<coverage_fill::FillMap>> = levels[group.clone()]
            .iter()
            .map(|l| fills.as_mut().and_then(|f| f.take_zoom(l.plan.zoom)))
            .collect();
        let group_levels: Vec<ExportLevelCtx<'_>> = levels[group.clone()]
            .iter()
            .zip(&group_fills)
            .map(|(l, fill)| ExportLevelCtx {
                fills: fill.as_ref(),
                ..*l
            })
            .collect();
        let t_group = crate::tile_trace::mark();
        zooms.extend(export_level_group(
            writer,
            LevelMemory {
                store: store.as_deref_mut(),
                budget: &mut *budget,
            },
            (&group_levels, gate),
            ckpt,
            progress.fail_level,
            progress,
            diagnostics,
        )?);
        crate::tile_trace::span("group", t_group, || {
            serde_json::json!({
                "levels": group_levels.iter().map(|l| l.level_idx).collect::<Vec<_>>(),
                "zooms": group_levels.iter().map(|l| l.plan.zoom).collect::<Vec<_>>(),
                "cap": gate.cap,
                "single_wave_cap": gate.single_wave_cap,
            })
        });
    }
    Ok(zooms)
}

/// The zoom a checkpoint after `level` covers, or `None` after the final
/// level, where `finalize` follows.
pub(super) fn checkpoint_zoom_after_level(level: &ExportLevelCtx<'_>) -> Option<u8> {
    (level.level_idx + 1 < level.num_levels).then_some(level.plan.zoom)
}

/// The test-only fault [`Pass2Knobs::fail_level`] injects as `level` starts.
pub(super) fn injected_fault(
    level: &ExportLevelCtx<'_>,
    fail_level: Option<usize>,
) -> Result<(), ExportError> {
    if fail_level == Some(level.level_idx) {
        return Err(ExportError::Io(std::io::Error::other(format!(
            "injected fault at level {}",
            level.level_idx
        ))));
    }
    Ok(())
}

/// Export a group of consecutive levels planned by [`plan_level_groups`]
/// (#647), encoding the group's levels, and their waves, concurrently.
///
/// A group of one single-wave level, or of one multi-wave level when
/// `gate.waves` is off (the serial oracle), is [`export_level`]
/// unchanged. Otherwise the calling thread collects every wave of every
/// level in order, as the serial loop does: the single-read store's wave,
/// a multi-wave level's one-level store (#740, filled as the level starts),
/// or one [`collect_wave`] read. Each collected wave then encodes in its own
/// Rayon task while the next one is collected, so a z3 tile no longer waits
/// for the z2 tiles to finish, and a wave's valve tail no longer idles the
/// pool until the next wave is read (#817).
///
/// [`InFlight`] bounds the members collected and not yet encoded, and under
/// `auto` and `speed` the encoded tiles still waiting to be written. A wave
/// of a multi-wave level also waits until every wave but the one before it
/// is written, so at most two of its waves' tiles wait on a slower coarser
/// one; under `bounded` a group's single-wave levels are bounded by the
/// group's sum.
///
/// A multi-wave level's one-level store fills as soon as the collect
/// reaches the level under `auto` and `speed`, before those waits, so the
/// level's band read and clip run while the coarser levels still encode
/// ([`GroupGate::overlap`]). Under `bounded` it fills once the group
/// holds no members, as the serial loop does.
///
/// The collect stays on the calling thread because the legacy read blocks on
/// a channel (see [`scoped_pipe`]). In a Rayon task it would park a worker,
/// and a worker waiting in a nested join could pick up another level's read
/// and block under it.
///
/// [`GroupCommit`] writes each wave as soon as it and every earlier wave
/// of the group are encoded, and takes a due checkpoint after each level.
/// The writer gets the same tiles in the same order as from the serial
/// loop, so the archive is byte-identical. A failed level leaves every
/// coarser level of the group written and, when due, checkpointed.
pub(super) fn export_level_group(
    writer: &mut StreamingPmtilesWriter,
    mem: LevelMemory<'_>,
    (levels, gate): (&[ExportLevelCtx<'_>], GroupGate),
    ckpt: &CheckpointCtx<'_>,
    fail_level: Option<usize>,
    progress: &mut ExportProgress,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<ZoomReport>, ExportError> {
    let LevelMemory { mut store, budget } = mem;
    if let [level] = levels {
        if !gate.waves || level_waves(level.plan) == 1 {
            injected_fault(level, fail_level)?;
            let report = export_level(writer, store, level, budget, progress, diagnostics)?;
            checkpoint_if_due(writer, ckpt, checkpoint_zoom_after_level(level), progress)?;
            return Ok(vec![report]);
        }
    }
    // A handle of its own, so the encode tasks can time themselves while
    // `GroupCommit` holds `progress` mutably.
    let timers = Arc::clone(&progress.profile.timers);
    let commit = Mutex::new(GroupCommit {
        writer,
        progress,
        ckpt,
        levels,
        next: (0, 0),
        written: 0,
        open: OpenLevel::default(),
        pending: BTreeMap::new(),
        freed: 0,
        reports: Vec::with_capacity(levels.len()),
        error: None,
    });
    let in_flight = InFlight {
        queue: QueueCharge::new(gate.overlap, levels[0].member_bytes),
        ..InFlight::new(if levels.iter().any(|level| level_waves(level.plan) > 1) {
            gate.cap
        } else {
            gate.single_wave_cap
        })
    };
    let collected = rayon::in_place_scope(|scope| -> Result<(), (usize, ExportError)> {
        // The group's ordinal of the next wave to collect.
        let mut unit = 0;
        'levels: for (i, level) in levels.iter().enumerate() {
            if lock_commit(&commit, &timers).error.is_some() {
                // A coarser level already failed. Its error is returned.
                break;
            }
            injected_fault(level, fail_level).map_err(|e| (i, e))?;
            let multi_wave = level_waves(level.plan) > 1;
            let mut level_store = None;
            let mut grants = Vec::new();
            for w in 0..level_waves(level.plan) {
                let wave = level.plan.wave(w);
                let fills = w == 0 && multi_wave && store.is_none() && level.level_read.is_some();
                let early = fills && gate.overlap;
                let mut fill = |floor: usize| -> Result<f64, (usize, ExportError)> {
                    let t_fill = Instant::now();
                    if let Some(read) = level.level_read {
                        let (backing, g) = level_read_backing(read, level.plan, budget, floor);
                        #[cfg(test)]
                        if let Some(wave) = g.first() {
                            level
                                .work
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .fill_grants
                                .push((in_flight.held(), wave.units));
                        }
                        grants = g;
                        let ctx = level.level_ctx(&timers);
                        level_store = Some(
                            fill_level_store(&ctx, level.plan, backing, diagnostics)
                                .map_err(|e| (i, e))?,
                        );
                        trace_fill(t_fill, level, early);
                    }
                    Ok(t_fill.elapsed().as_secs_f64())
                };
                // Under `auto` and `speed` the store fills at once, while the
                // levels before it still encode: the gate keeps their members
                // and this level's waves within its cap together, so the
                // store's wave grant is the cap, not this level's widest wave.
                // A wave the gate let past the cap because nothing else was
                // held can still be in flight: the grant then covers it, and
                // no wave of this level is admitted until it is released.
                let fill_start = Instant::now();
                let fill_secs = if early {
                    fill(gate.cap.max(in_flight.held()))?
                } else {
                    0.0
                };
                if multi_wave {
                    gate_wait(("window_wait", level, w), &in_flight, || {
                        in_flight.wait_until(|| {
                            let c = lock_commit(&commit, &timers);
                            c.error.is_some() || c.written + 1 >= unit
                        });
                    });
                    if lock_commit(&commit, &timers).error.is_some() {
                        break 'levels;
                    }
                }
                if fills && !early {
                    // The store's grant is sized beside this level's widest
                    // wave only, so it fills once every earlier wave is
                    // encoded: the grant plus one wave, as in the serial loop.
                    gate_wait(("drain_wait", level, w), &in_flight, || {
                        drop(in_flight.wait_for(|held| held == 0));
                    });
                }
                let held = gate_wait(("admit_wait", level, w), &in_flight, || {
                    in_flight.admit(wave.iter().map(|p| p.members).sum())
                });
                // The level's wall starts with its fill.
                let start = if early { fill_start } else { Instant::now() };
                let t_collect = Instant::now();
                if fills && !early {
                    fill(0)?;
                }
                let spill_bytes = match (&level_store, &store) {
                    _ if w > 0 => 0,
                    (Some(s), _) => s.level_spilled_bytes(0),
                    (None, Some(s)) => s.level_spilled_bytes(level.level_idx),
                    (None, None) => 0,
                };
                let own = level_store.is_some();
                let buckets = collect_unit(
                    level_store.as_mut().or(store.as_deref_mut()),
                    (level, own),
                    (w, wave),
                    &timers,
                    diagnostics,
                )
                .map_err(|e| (i, e))?;
                trace_wave("collected", level, w);
                // The fill is timed on its own: an early one does not count
                // the waits between it and the wave's admission.
                let collect_secs = t_collect.elapsed().as_secs_f64() + fill_secs;
                #[cfg(test)]
                if multi_wave {
                    level
                        .work
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .group_waves += 1;
                }
                #[cfg(test)]
                let next_fill = (w + 1 == level_waves(level.plan))
                    .then(|| levels.get(i + 1))
                    .flatten()
                    .filter(|next| level_waves(next.plan) > 1 && store.is_none())
                    .map(|next| next.level_idx);
                let (commit, timers, in_flight) = (&commit, &*timers, &in_flight);
                trace_wave("spawned", level, w);
                scope.spawn(move |_| {
                    wave_job(level, w, || {
                        #[cfg(test)]
                        if let Some(next) = next_fill {
                            hold_for_fill(level.work, next);
                        }
                        #[cfg(test)]
                        if (i, w) == (0, 0) {
                            stall_first_wave(level.work, commit, levels.len());
                        }
                        let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            let t_encode = Instant::now();
                            let tiles = encode_buckets(
                                level.gzip_cache,
                                (buckets, Some(&held)),
                                level.plan.zoom,
                                &TileEncoding::single(level.options).with_floors(level.floors),
                                level.fills,
                                timers,
                                diagnostics,
                            );
                            let encoded_ns = trace_wave("encoded", level, w);
                            // The tiles' share of the cap goes with them until
                            // written; a failed wave's goes back with `held`.
                            let queued = if tiles.is_ok() { held.take_queued() } else { 0 };
                            drop(held);
                            let encoded = tiles.map(|tiles| EncodedWave {
                                tiles,
                                start,
                                collect_secs,
                                encode_secs: t_encode.elapsed().as_secs_f64(),
                                spill_bytes,
                                queued,
                                encoded_ns,
                            });
                            lock_commit(commit, timers).finish((i, w), encoded);
                        }));
                        let freed = {
                            let mut c = lock_commit(commit, timers);
                            if run.is_err() {
                                // The collect may wait on this wave's write: fail
                                // the group so it stops, then let Rayon re-raise.
                                let e = std::io::Error::other("a wave's encode or write panicked");
                                c.fail(i, ExportError::Io(e));
                            }
                            std::mem::take(&mut c.freed)
                        };
                        in_flight.release(freed);
                        in_flight.notify();
                        if let Err(panic) = run {
                            std::panic::resume_unwind(panic);
                        }
                    })
                });
                unit += 1;
            }
            // The level's waves are all taken: its one-level store is empty.
            drop(level_store);
            for grant in grants {
                budget.release(grant);
            }
        }
        Ok(())
    });
    let mut commit = commit.into_inner().unwrap_or_else(PoisonError::into_inner);
    if let Err((i, e)) = collected {
        commit.fail(i, e);
    }
    commit.into_reports()
}

/// [`Pass2Knobs::hold_for_fill`]: wait up to 2 s for level `next`'s store
/// to start filling, and count it in
/// [`ExportWorkTally::fills_beside_encode`] if it does. The start, not the
/// end: the fill's Rayon work may need the worker this hold occupies.
#[cfg(test)]
pub(super) fn hold_for_fill(work: &Mutex<ExportWorkTally>, next: usize) {
    let lock = || work.lock().unwrap_or_else(PoisonError::into_inner);
    if !lock().hold_for_fill {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        let mut work = lock();
        if work.fills_started.contains(&next) {
            work.fills_beside_encode += 1;
            return;
        }
        drop(work);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// [`Pass2Knobs::stall_first_wave`]: wait up to 2 s for every later wave of
/// a group of `waves` single-wave levels to wait on this one.
#[cfg(test)]
pub(super) fn stall_first_wave(
    work: &Mutex<ExportWorkTally>,
    commit: &Mutex<GroupCommit<'_>>,
    waves: usize,
) {
    if !work
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .stall_first_wave
    {
        return;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if commit
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pending
            .len()
            + 1
            >= waves
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// #876: a wave's identity and size in the trace.
pub(super) fn wave_fields(level: &ExportLevelCtx<'_>, w: usize) -> serde_json::Value {
    let wave = level.plan.wave(w);
    serde_json::json!({
        "level": level.level_idx,
        "zoom": level.plan.zoom,
        "wave": w,
        "members": wave.iter().map(|p| p.members).sum::<usize>(),
        "partitions": wave.len(),
    })
}

/// #876: a `wave` lifecycle event, and when it was, if tracing.
pub(super) fn trace_wave(stage: &str, level: &ExportLevelCtx<'_>, w: usize) -> Option<u64> {
    let t = crate::tile_trace::mark();
    crate::tile_trace::event("wave", || {
        let mut v = wave_fields(level, w);
        v["stage"] = stage.into();
        v
    });
    t
}

/// #876: run wave `w`'s encode task as a traced job, its tiles placed in
/// the wave.
pub(super) fn wave_job<R>(level: &ExportLevelCtx<'_>, w: usize, task: impl FnOnce() -> R) -> R {
    let ctx = || crate::tile_trace::Ctx::wave(level.level_idx, level.plan.zoom, w);
    crate::tile_trace::job(ctx, || {
        trace_wave("started", level, w);
        task()
    })
}

/// #876: run a collect's `wait` on the group's gate as trace event `ev`,
/// with the members `held` when it ended (an admission's included).
pub(super) fn gate_wait<R>(
    (ev, level, w): (&'static str, &ExportLevelCtx<'_>, usize),
    gate: &InFlight,
    wait: impl FnOnce() -> R,
) -> R {
    let start = crate::tile_trace::mark();
    let r = wait();
    crate::tile_trace::span(ev, start, || {
        let mut v = wave_fields(level, w);
        v["held"] = gate.held().into();
        v["cap"] = gate.cap.into();
        v
    });
    r
}

/// #876: a level's one-level store fill, started at `t_fill`.
pub(super) fn trace_fill(t_fill: Instant, level: &ExportLevelCtx<'_>, early: bool) {
    let start = crate::tile_trace::mark().map(|now| {
        now.saturating_sub(u64::try_from(t_fill.elapsed().as_nanos()).unwrap_or(u64::MAX))
    });
    crate::tile_trace::span("fill", start, || {
        serde_json::json!({
            "kind": "level_store",
            "level": level.level_idx,
            "zoom": level.plan.zoom,
            "early": early,
        })
    });
}

/// How a level group runs ([`export_level_group`]): the [`InFlight`] cap
/// ([`group_schedule`]) of a group with a multi-wave level and of one with
/// only single-wave levels, and whether a multi-wave level's waves overlap.
#[derive(Clone, Copy)]
pub(super) struct GroupGate {
    pub(super) cap: usize,
    pub(super) single_wave_cap: usize,
    pub(super) waves: bool,
    /// `auto` and `speed`: fill a multi-wave level's one-level store before
    /// waiting for the levels before it, its wave grant sized at `cap`, or at
    /// the members still in flight when an oversized wave holds more; and
    /// keep each encoded wave's share of the cap until it is written
    /// ([`EncodedWave::queued`]).
    pub(super) overlap: bool,
}

/// The waves `plan` exports in, at least one: an empty level writes its
/// stopped tiles and reports as one empty wave.
pub(super) fn level_waves(plan: &LevelPlan) -> usize {
    plan.wave_count().max(1)
}

/// The members a level group has admitted to collect and not yet encoded
/// (#817, see [`group_schedule`]). The collecting thread waits in
/// [`Self::admit`] while the next level would take them past `cap`, unless
/// none are in flight, so a level larger than the cap still runs.
///
/// Under `auto` and `speed` a finer level can finish while a coarser wave
/// still encodes, and its tiles wait for that wave's write. Each encoded
/// partition then keeps [`Self::queued_share`] of the cap in place of its
/// members until [`GroupCommit`] writes it, so a stalled coarse wave bounds
/// the output queued behind it as well as the members.
pub(super) struct InFlight {
    pub(super) cap: usize,
    pub(super) held: Mutex<usize>,
    pub(super) freed: std::sync::Condvar,
    /// What an encoded partition keeps of the cap until written.
    pub(super) queue: QueueCharge,
}

/// What an encoded partition keeps of a level group's [`InFlight`] cap until
/// its wave is written ([`InFlight::queued_share`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueueCharge {
    /// `bounded`: nothing; its group's summed members bound the queue.
    Off,
    /// The partition's members: the member price is unknown.
    Members,
    /// The tiles' bytes at this many bytes a member, the price the cap was
    /// sized at.
    Bytes(u64),
}

impl QueueCharge {
    pub(super) fn new(overlap: bool, member_bytes: Option<u64>) -> Self {
        match (overlap, member_bytes.filter(|&b| b > 0)) {
            (false, _) => Self::Off,
            (true, None) => Self::Members,
            (true, Some(price)) => Self::Bytes(price),
        }
    }
}

/// A level's admitted members. Each partition's are released as it
/// encodes ([`Self::encoded`]), so the next level's collect overlaps the
/// encode's tail; the rest when its encode task drops it, panicking or not.
pub(super) struct Admitted<'g> {
    pub(super) gate: &'g InFlight,
    pub(super) remaining: Mutex<usize>,
    /// The encoded partitions' share of the cap, not yet taken by
    /// [`Self::take_queued`].
    pub(super) queued: Mutex<usize>,
}

impl Admitted<'_> {
    /// Release up to `members` of the admitted members (a partition's
    /// collected count can differ from its planned one) once they encoded
    /// to `tiles`, keeping their [`InFlight::queued_share`] of the cap in
    /// the same step.
    pub(super) fn encoded(&self, members: usize, tiles: &[EncodedTile]) {
        let mut remaining = self
            .remaining
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let freed = members.min(*remaining);
        *remaining -= freed;
        let share = self.gate.queued_share(freed, tiles);
        *self.queued.lock().unwrap_or_else(PoisonError::into_inner) += share;
        self.gate.settle(freed, share);
    }

    /// The encoded partitions' share of the cap, which the caller now
    /// releases once their tiles are written.
    pub(super) fn take_queued(&self) -> usize {
        std::mem::take(&mut *self.queued.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

impl InFlight {
    pub(super) fn new(cap: usize) -> Self {
        Self {
            cap,
            held: Mutex::new(0),
            freed: std::sync::Condvar::new(),
            queue: QueueCharge::Off,
        }
    }

    /// What a partition of `members` encoded to `tiles` keeps of the cap
    /// until written: the tiles' bytes at the member price, or its members
    /// when the price is unknown, since gzipped MVT bytes are much smaller
    /// than the members they come from. Nothing for no tiles, or under
    /// [`QueueCharge::Off`].
    pub(super) fn queued_share(&self, members: usize, tiles: &[EncodedTile]) -> usize {
        match self.queue {
            _ if tiles.is_empty() => 0,
            QueueCharge::Off => 0,
            QueueCharge::Members => members,
            QueueCharge::Bytes(price) => {
                let bytes: u64 = tiles
                    .iter()
                    .map(|t| (t.data.len() + std::mem::size_of::<EncodedTile>()) as u64)
                    .sum();
                usize::try_from(bytes.div_ceil(price)).unwrap_or(usize::MAX)
            }
        }
    }

    /// Wait until `ready(held)`, re-checked whenever members are released or
    /// [`Self::notify`] is called, and return the lock.
    pub(super) fn wait_for(
        &self,
        mut ready: impl FnMut(usize) -> bool,
    ) -> std::sync::MutexGuard<'_, usize> {
        let lock = || self.held.lock().unwrap_or_else(PoisonError::into_inner);
        let mut held = lock();
        while !ready(*held) {
            if rayon::current_thread_index().is_some() {
                // A pool thread runs the encodes it waits on, never parks.
                drop(held);
                if rayon::yield_now() != Some(rayon::Yield::Executed) {
                    std::thread::yield_now();
                }
                held = lock();
            } else {
                held = self
                    .freed
                    .wait(held)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        held
    }

    pub(super) fn admit(&self, members: usize) -> Admitted<'_> {
        let mut held = self.wait_for(|h| h == 0 || h.saturating_add(members) <= self.cap);
        *held += members;
        drop(held);
        Admitted {
            gate: self,
            remaining: Mutex::new(members),
            queued: Mutex::new(0),
        }
    }

    /// Wait until `ready()`: a condition another thread changes before it
    /// calls [`Self::notify`]. `ready` runs under this gate's lock.
    pub(super) fn wait_until(&self, mut ready: impl FnMut() -> bool) {
        drop(self.wait_for(|_| ready()));
    }

    /// The members admitted and not yet released.
    pub(super) fn held(&self) -> usize {
        *self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn notify(&self) {
        let _held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        self.freed.notify_all();
    }

    pub(super) fn release(&self, members: usize) {
        self.settle(members, 0);
    }

    /// Release `freed` members and take `kept` in one step, so a waiting
    /// admission never sees the gate between the two.
    pub(super) fn settle(&self, freed: usize, kept: usize) {
        if freed == kept {
            return;
        }
        let mut held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        *held = *held - freed + kept;
        self.freed.notify_all();
    }
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        let remaining = *self.remaining.get_mut().unwrap_or_else(|e| e.into_inner());
        // A wave whose share was not taken never reaches the write.
        let queued = *self.queued.get_mut().unwrap_or_else(|e| e.into_inner());
        self.gate.release(remaining + queued);
    }
}

/// Lock the group's [`GroupCommit`]. A poisoned lock means an encode task
/// panicked, and Rayon re-raises that panic when the group's scope ends, so
/// the state is used as is. The wait is charged to `group_commit_wait`.
pub(super) fn lock_commit<'g, 'a>(
    commit: &'g Mutex<GroupCommit<'a>>,
    timers: &ExportTimers,
) -> std::sync::MutexGuard<'g, GroupCommit<'a>> {
    ExportTimers::time(&timers.group_commit_wait, || {
        commit.lock().unwrap_or_else(PoisonError::into_inner)
    })
}

/// Collect wave `w` of `level`, bucketed per partition: from `store` (the
/// level's own one-level store when `own`, else the single-read store), or
/// one [`collect_wave`] read. An empty wave has no buckets.
pub(super) fn collect_unit(
    store: Option<&mut MemberStore>,
    (level, own): (&ExportLevelCtx<'_>, bool),
    (w, wave): (usize, &[Partition]),
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<Vec<Member>>, ExportError> {
    if wave.is_empty() {
        return Ok(Vec::new());
    }
    match store {
        Some(s) => {
            let store_level = if own { 0 } else { level.level_idx };
            let members = ExportTimers::time(&timers.spill_read, || s.take_wave(store_level, w))?;
            Ok(bucket_members(members, wave))
        }
        None => Ok(collect_wave(&level.level_ctx(timers), wave, diagnostics)?.buckets),
    }
}

/// One encoded wave of a group, waiting for [`GroupCommit`] to write it.
pub(super) struct EncodedWave {
    pub(super) tiles: Vec<Vec<EncodedTile>>,
    /// When the wave's collect started: its level's wall in the profile runs
    /// from its first wave's start to the end of its write.
    pub(super) start: Instant,
    /// The wave's collect time, on the calling thread.
    pub(super) collect_secs: f64,
    /// The wave's encode time, in its Rayon task.
    pub(super) encode_secs: f64,
    /// #810: the level's spilled member bytes (on its first wave).
    pub(super) spill_bytes: u64,
    /// The members of the group's [`InFlight`] cap the encoded tiles keep
    /// until they are written ([`InFlight::queued_share`]).
    pub(super) queued: usize,
    /// #876: when the encode finished, when tracing.
    pub(super) encoded_ns: Option<u64>,
}

/// The level [`GroupCommit`] is writing: its tally and times so far.
#[derive(Default)]
pub(super) struct OpenLevel {
    pub(super) tally: LevelTally,
    pub(super) start: Option<Instant>,
    pub(super) collect_secs: f64,
    pub(super) encode_secs: f64,
    pub(super) last_log: Option<Instant>,
}

/// The ordered write side of a concurrent level group (#647). Encode tasks
/// hand their waves to [`Self::finish`] in any order. Waves are written in
/// (level, wave) order, each as soon as every earlier wave of the group is
/// written; a level is reported and checkpointed after its last wave.
pub(super) struct GroupCommit<'a> {
    pub(super) writer: &'a mut StreamingPmtilesWriter,
    pub(super) progress: &'a mut ExportProgress,
    pub(super) ckpt: &'a CheckpointCtx<'a>,
    pub(super) levels: &'a [ExportLevelCtx<'a>],
    /// The (group level index, wave) of the next wave to write.
    pub(super) next: (usize, usize),
    /// Waves written so far.
    pub(super) written: usize,
    pub(super) open: OpenLevel,
    /// Encoded waves that wait for an earlier one.
    pub(super) pending: BTreeMap<(usize, usize), EncodedWave>,
    /// The [`EncodedWave::queued`] members of waves written or dropped
    /// since an encode task last released them to the group's [`InFlight`].
    pub(super) freed: usize,
    pub(super) reports: Vec<ZoomReport>,
    /// The failure at the lowest group index so far. No level at or after
    /// it is written.
    pub(super) error: Option<(usize, ExportError)>,
}

impl GroupCommit<'_> {
    /// Take one wave's encode result, then write every wave that is ready.
    pub(super) fn finish(
        &mut self,
        unit: (usize, usize),
        encoded: Result<EncodedWave, ExportError>,
    ) {
        match encoded {
            Ok(wave) => {
                if self.error.as_ref().is_some_and(|(j, _)| unit.0 >= *j) {
                    // Never written: its share of the gate goes back now.
                    self.freed += wave.queued;
                    return;
                }
                self.pending.insert(unit, wave);
                self.write_ready();
                #[cfg(test)]
                {
                    let mut work = self.levels[0]
                        .work
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    work.peak_queued_waves = work.peak_queued_waves.max(self.pending.len());
                }
            }
            Err(e) => self.fail(unit.0, e),
        }
    }

    /// Record a failure at group index `i`, keeping the coarsest one.
    pub(super) fn fail(&mut self, i: usize, e: ExportError) {
        if self.error.as_ref().is_none_or(|(j, _)| i < *j) {
            self.error = Some((i, e));
            // No wave at or after it is written: free their share of the gate.
            let dropped = self.pending.split_off(&(i, 0));
            self.freed += dropped.values().map(|wave| wave.queued).sum::<usize>();
        }
    }

    pub(super) fn write_ready(&mut self) {
        while self.error.as_ref().is_none_or(|(j, _)| self.next.0 < *j) {
            let Some(wave) = self.pending.remove(&self.next) else {
                return;
            };
            self.freed += wave.queued;
            if let Err(e) = self.write_wave(wave) {
                self.fail(self.next.0, e);
            }
        }
    }

    /// Write one wave's tiles. After a level's last wave, report the level
    /// and take a checkpoint when due.
    pub(super) fn write_wave(&mut self, encoded: EncodedWave) -> Result<(), ExportError> {
        let (i, w) = self.next;
        let level = &self.levels[i];
        #[cfg(test)]
        assert!(
            w > 0 || self.progress.panic_write_level != Some(level.level_idx),
            "injected write panic at level {}",
            level.level_idx
        );
        let open = &mut self.open;
        if w == 0 {
            *open = OpenLevel {
                tally: LevelTally {
                    spill_bytes: encoded.spill_bytes,
                    ..LevelTally::default()
                },
                start: Some(encoded.start),
                last_log: Some(Instant::now()),
                ..OpenLevel::default()
            };
        }
        open.collect_secs += encoded.collect_secs;
        open.encode_secs += encoded.encode_secs;
        crate::tile_trace::span("commit_wait", encoded.encoded_ns, || {
            let mut v = wave_fields(level, w);
            v["queued"] = encoded.queued.into();
            v
        });
        let timers = &self.progress.profile.timers;
        let zoom = level.plan.zoom;
        write_encoded_tiles(
            self.writer,
            zoom,
            &encoded.tiles,
            level.terminals,
            &mut open.tally,
            timers,
        )?;
        drop(encoded.tiles);
        trace_wave("written", level, w);
        self.written += 1;
        let waves = level_waves(level.plan);
        if w + 1 < waves {
            self.next = (i, w + 1);
            if open
                .last_log
                .is_some_and(|t| t.elapsed() >= WAVE_LOG_INTERVAL)
            {
                log::info!(target: LOG_TARGET,
                    "[export] level {}/{} z{zoom}: wave {}/{waves}, {} tiles, {:.0}s",
                    level.level_idx + 1,
                    level.num_levels,
                    w + 1,
                    open.tally.tile_count,
                    level.start.elapsed().as_secs_f64(),
                );
                open.last_log = Some(Instant::now());
            }
            return Ok(());
        }
        write_terminals(
            self.writer,
            zoom,
            level.terminals,
            None,
            &mut open.tally,
            timers,
        )?;
        let open = std::mem::take(&mut self.open);
        let report = finish_level(
            level,
            &open.tally,
            level.plan.wave_count(),
            LevelWalls::grouped(
                open.start.map_or(0.0, |s| s.elapsed().as_secs_f64()),
                open.collect_secs,
                open.encode_secs,
            ),
            self.progress,
        );
        checkpoint_if_due(
            self.writer,
            self.ckpt,
            checkpoint_zoom_after_level(level),
            self.progress,
        )?;
        self.reports.push(report);
        self.next = (i + 1, 0);
        Ok(())
    }

    /// The group's reports in level order, or the coarsest failure.
    pub(super) fn into_reports(self) -> Result<Vec<ZoomReport>, ExportError> {
        if let Some((_, e)) = self.error {
            return Err(e);
        }
        debug_assert_eq!(self.reports.len(), self.levels.len());
        Ok(self.reports)
    }
}

/// A level's wall time and its own work time, for its profile entry and
/// log lines.
pub(super) struct LevelWalls {
    /// From the level's start to the end of its write. Levels of one group
    /// overlap, so these do not add up to `phase_walls.levels`.
    pub(super) wall_secs: f64,
    /// Read, decode, clip, encode and gzip, without the write or any wait.
    pub(super) work_secs: f64,
}

impl LevelWalls {
    /// A level of the serial wave loop: all of its wall but the write.
    pub(super) fn serial(wall_secs: f64, write_secs: f64) -> Self {
        Self {
            wall_secs,
            work_secs: wall_secs - write_secs,
        }
    }

    /// A level of a concurrent group: its collect and its encode. The time
    /// it waits for a coarser level, and its write, are not work.
    pub(super) fn grouped(wall_secs: f64, collect_secs: f64, encode_secs: f64) -> Self {
        Self {
            wall_secs,
            work_secs: collect_secs + encode_secs,
        }
    }
}

/// One level's running totals while its tiles go to the writer.
#[derive(Default)]
pub(super) struct LevelTally {
    pub(super) tile_count: usize,
    pub(super) tile_feature_count: usize,
    pub(super) oversized: usize,
    pub(super) encode_dropped: usize,
    pub(super) encode_quantized: usize,
    pub(super) write_secs: f64,
    pub(super) bytes_written: u64,
    /// The size valve's per-tile tallies, summed (#745): `merged` and
    /// `dropped` add up; `passes` and `mingap` are the largest seen.
    pub(super) valve: ValveTally,
    /// Every tile's `valve.passes`, summed (#750).
    pub(super) valve_passes: u64,
    /// Every tile's coverage fill, summed (#750).
    pub(super) fill: FillTally,
    /// #810: the level's spilled member bytes, for its profile entry.
    pub(super) spill_bytes: u64,
    /// #803: the level's stopped tiles written so far.
    pub(super) terminals: usize,
    /// The largest tiles the valve left over the limit, and how many of
    /// those tiles hold merged features (#873).
    pub(super) over_limit_largest: Vec<crate::stats::LargestTile>,
    pub(super) over_limit_merged: usize,
}

/// Append one wave's encoded tiles to the writer in order, and add them to
/// `tally`. The level's #803 stopped tiles that sort before each are
/// written first, so the archive stays in tile-id order.
pub(super) fn write_encoded_tiles(
    writer: &mut StreamingPmtilesWriter,
    zoom: u8,
    results: &[Vec<EncodedTile>],
    terminals: &[EncodedTile],
    tally: &mut LevelTally,
    timers: &ExportTimers,
) -> Result<(), ExportError> {
    for t in results.iter().flatten() {
        write_terminals(
            writer,
            zoom,
            terminals,
            Some(tile_key(t.x, t.y, zoom)),
            tally,
            timers,
        )?;
        write_tile(writer, zoom, t, tally, timers)?;
    }
    Ok(())
}

/// Write the level's #803 stopped tiles not yet written whose key is below
/// `before` (all of them for `None`).
pub(super) fn write_terminals(
    writer: &mut StreamingPmtilesWriter,
    zoom: u8,
    terminals: &[EncodedTile],
    before: Option<u64>,
    tally: &mut LevelTally,
    timers: &ExportTimers,
) -> Result<(), ExportError> {
    while let Some(t) = terminals.get(tally.terminals) {
        if before.is_some_and(|key| tile_key(t.x, t.y, zoom) >= key) {
            break;
        }
        write_tile(writer, zoom, t, tally, timers)?;
        tally.terminals += 1;
    }
    Ok(())
}

/// Append one encoded tile to the writer, and add it to `tally`.
pub(super) fn write_tile(
    writer: &mut StreamingPmtilesWriter,
    zoom: u8,
    t: &EncodedTile,
    tally: &mut LevelTally,
    timers: &ExportTimers,
) -> Result<(), ExportError> {
    let t_write = Instant::now();
    tally.encode_dropped += t.encode_dropped;
    tally.encode_quantized += t.encode_quantized;
    tally.valve.merged += t.valve.merged;
    tally.valve.dropped += t.valve.dropped;
    tally.valve.dropped_merged += t.valve.dropped_merged;
    tally.valve.dropped_merged_sources += t.valve.dropped_merged_sources;
    tally.valve.passes = tally.valve.passes.max(t.valve.passes);
    tally.valve_passes += u64::from(t.valve.passes);
    tally.fill.polygons += t.fill.polygons;
    tally.fill.vertices += t.fill.vertices;
    tally.fill.shed += t.fill.shed;
    tally.valve.mingap = tally.valve.mingap.max(t.valve.mingap);
    if t.feature_count > 0 {
        if let Some(extent) = t.valve.reduced_extent {
            tally.valve.reduced_tiles += 1;
            tally.valve.reduced_extent =
                Some(tally.valve.reduced_extent.map_or(extent, |e| e.min(extent)));
        }
        // #431: an all-dropped tile is a tally carrier, not a tile.
        tally.tile_feature_count += t.feature_count;
        tally.bytes_written += t.data.len() as u64;
        if t.oversized {
            tally.oversized += 1;
        }
        tally.valve.over_limit += t.valve.over_limit;
        if t.valve.over_limit > 0 {
            let tile = crate::stats::LargestTile {
                z: zoom,
                x: t.x,
                y: t.y,
                bytes: t.data.len() as u64,
            };
            crate::stats::keep_largest(&mut tally.over_limit_largest, [tile]);
            tally.over_limit_merged += usize::from(t.valve.over_limit_merged);
        }
        writer.add_tile_precompressed(
            crate::tile::TileCoord {
                z: zoom,
                x: t.x,
                y: t.y,
            },
            t.hash,
            &t.data,
            t.raw_len,
            t.feature_count,
        )?;
        tally.tile_count += 1;
    }
    let write_dur = t_write.elapsed();
    tally.write_secs += write_dur.as_secs_f64();
    // #535: `spool_write` is exactly the serial per-tile
    // `writer.add_tile_precompressed` window, the span `write_secs` measures
    // for the `[profile]` log.
    add_nanos(&timers.spool_write, write_dur);
    Ok(())
}

/// Log a written level, record its profile entry, and build its report.
pub(super) fn finish_level(
    level: &ExportLevelCtx<'_>,
    tally: &LevelTally,
    total_waves: usize,
    walls: LevelWalls,
    progress: &mut ExportProgress,
) -> ZoomReport {
    let LevelWalls {
        wall_secs,
        work_secs,
    } = walls;
    let ExportLevelCtx {
        scan,
        plan,
        level_idx,
        num_levels,
        start,
        ..
    } = *level;
    let zoom = plan.zoom;
    let tile_count = tally.tile_count;
    // Per-level summary at info so operators see progress without RUST_LOG
    // (env_logger defaults to info). Detailed clip/write split stays debug.
    log::info!(target: LOG_TARGET,
        "[export] level {}/{num_levels} z{zoom} done: {} feats, {tile_count} tiles, \
         {} partitions, {wall_secs:.1}s (total {:.0}s)",
        level_idx + 1,
        scan.feature_count,
        plan.partitions.len(),
        start.elapsed().as_secs_f64(),
    );
    log::debug!(target: LOG_TARGET,
        "[profile] z{zoom} (level {level_idx}, {} feats, {tile_count} tiles, {} partitions): \
         clip+encode+gzip={work_secs:.2}s write={:.2}s",
        scan.feature_count,
        plan.partitions.len(),
        tally.write_secs,
    );

    let profile = &mut progress.profile;
    profile.per_zoom.push(ExportZoomProfile {
        zoom,
        wall_secs,
        tiles: tile_count,
        features: tally.tile_feature_count,
        bytes: tally.bytes_written,
        wave_width: plan.widest_wave().max(1),
        spill_bytes: tally.spill_bytes,
    });
    profile.waves_total += total_waves;
    progress.sample_boundary_rss();
    ZoomReport {
        zoom,
        level: level_idx,
        level_feature_count: scan.feature_count,
        tile_count,
        tile_feature_count: tally.tile_feature_count,
        oversized_tiles: tally.oversized,
        over_limit_tiles: tally.valve.over_limit,
        encode_dropped_features: tally.encode_dropped,
        encode_quantized_features: tally.encode_quantized,
        merged_features: tally.valve.merged,
        valve_dropped_features: tally.valve.dropped,
        max_valve_passes: tally.valve.passes,
        valve_passes: tally.valve_passes,
        fill_polygons: tally.fill.polygons,
        fill_vertices: tally.fill.vertices,
        fill_shed: tally.fill.shed,
        stopped_tiles: tally.terminals,
        over_limit_largest: tally.over_limit_largest.clone(),
        over_limit_merged: tally.over_limit_merged,
        reduced_extent_tiles: tally.valve.reduced_tiles,
        min_reduced_extent: tally.valve.reduced_extent,
        dropped_merged_features: tally.valve.dropped_merged,
        dropped_merged_sources: tally.valve.dropped_merged_sources,
    }
}

// ============================================================================
// Partitioned tiling + encoding (H3(b))
// ============================================================================

/// A tile's key in the per-zoom scan/partition/emit maps: its PMTiles tile id
/// ([`crate::pmtiles_writer::tile_id`]), i.e. its position on the zoom's
/// Hilbert curve plus the cumulative base of shallower zooms.
///
/// Every per-zoom `BTreeMap<u64, _>` this module keys on `tile_key` — the
/// scan pass's tile counts, partition `[key_lo, key_hi]` windows, member
/// routing — therefore walks tiles in ascending PMTiles tile-id order, and
/// `export_level` writes them to the archive in exactly that order (levels
/// export in ascending zoom, and every id at zoom `z` is less than every id
/// at zoom `z + 1`). That is what makes the export archive genuinely
/// `clustered: true` (#506) — the derived header flag from #501 comes out
/// true because the bytes really are laid out that way, not because it is
/// asserted.
///
/// DIVERGENCE FROM the pre-#506 key: this used to be the row-major pack
/// `(x << 32) | y`, chosen only to preserve a specific historical byte
/// layout. Switching to the tile id changes that layout (tile data now lands
/// in Hilbert order, which is spatially compact rather than column-striped)
/// but is otherwise a drop-in replacement — every consumer only ever compared
/// keys or looked one up, never split it into `x`/`y` bit-fields, except
/// [`plan_partitions`] and [`encode_members`]/[`tile_mvt`] (test-only), which
/// now decode a key back to `(x, y)` via
/// [`crate::pmtiles_writer::tile_id_to_zxy`] instead of unpacking bits.
#[inline]
pub(super) fn tile_key(x: u32, y: u32, zoom: u8) -> u64 {
    crate::pmtiles_writer::tile_id(zoom, x, y)
}

/// Inverse of [`tile_key`]: recover a tile's `(x, y)` at the zoom the key was
/// built for. `zoom` is the caller's own authority (the per-zoom map's zoom),
/// used only to `debug_assert` the id actually decodes to it — every key this
/// module produces came from `tile_key(_, _, zoom)`, so a mismatch would mean
/// a key leaked from the wrong zoom's map.
#[inline]
pub(super) fn key_to_xy(key: u64, zoom: u8) -> (u32, u32) {
    let (z, x, y) = crate::pmtiles_writer::tile_id_to_zxy(key)
        .expect("tile_key output must always decode back to a valid tile id");
    debug_assert_eq!(z, zoom, "key {key} decoded to zoom {z}, expected {zoom}");
    (x, y)
}
