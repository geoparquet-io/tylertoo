use super::*;

/// Pass 2, one wave of partitions: **read the band once** (row groups pruned to
/// the wave's combined bbox), clip every feature into its tiles across the
/// wave's whole key range, and *route* each member to its owning partition.
/// Each partition is then grouped-by-tile and MVT-encoded in parallel exactly
/// as a standalone partition would be. Returns one `Vec<EncodedTile>` per
/// partition, in the wave's ascending partition order, each in ascending
/// PMTiles tile-id order.
///
/// This replaces the old per-partition re-read (issue #228): instead of `P`
/// independent band reads + decodes per level, a wave of `partition_wave`
/// partitions shares a single read + decode. Byte-identical to the per-partition
/// path — each partition receives exactly the members with key in its
/// `[key_lo, key_hi]` window (a tile's clipped geometry is window-independent),
/// and within-tile `(key, seq)` order is preserved because row-group pruning
/// keeps kept rows in band order regardless of how wide the read is.
pub(super) fn process_wave(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<Vec<EncodedTile>>, ExportError> {
    let CollectedWave {
        buckets,
        rows_read,
        collect_secs,
    } = collect_wave(ctx, wave, diagnostics)?;
    let n_members: usize = buckets.iter().map(Vec::len).sum();

    let t_encode = Instant::now();
    let tiles = encode_buckets(
        ctx.gzip_cache,
        (buckets, None),
        ctx.zoom,
        &TileEncoding::single(ctx.opts).with_floors(ctx.floors),
        ctx.fills,
        ctx.timers,
        diagnostics,
    )?;
    let n_tiles: usize = tiles.iter().map(Vec::len).sum();
    log::debug!(target: LOG_TARGET,
        "[profile]     z{} wave [{:x}..{:x}] ({} partitions): rows_read={rows_read} \
         members={n_members} tiles={n_tiles} collect={collect_secs:.2}s \
         sort+encode={:.2}s",
        ctx.zoom,
        wave[0].key_lo,
        wave[wave.len() - 1].key_hi,
        wave.len(),
        t_encode.elapsed().as_secs_f64(),
    );
    Ok(tiles)
}

/// The positions and row-group context of a spatially pruned wave read.
pub(super) struct WaveRows {
    pub(super) read: u64,
    pub(super) gaps: RowGapCursor,
    pub(super) locator: crate::input_set::StreamRowLocator,
}

/// The read half of [`process_wave`]: the wave's members, bucketed per
/// partition.
pub(super) struct CollectedWave {
    pub(super) buckets: Vec<Vec<Member>>,
    pub(super) rows_read: u64,
    pub(super) collect_secs: f64,
}

/// Read the band once for a wave and clip and route its members, without
/// encoding them. See [`process_wave`].
pub(super) fn collect_wave(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<CollectedWave, ExportError> {
    let t_collect = Instant::now();
    let mut buckets: Vec<Vec<Member>> = (0..wave.len()).map(|_| Vec::new()).collect();
    let rows_read = collect_into(ctx, wave, &mut buckets, diagnostics)?;
    Ok(CollectedWave {
        buckets,
        rows_read,
        collect_secs: t_collect.elapsed().as_secs_f64(),
    })
}

/// A level of several waves, read once (#740): its whole band, pruned to all
/// of its partitions, clipped and routed into a one-level [`MemberStore`]
/// (level index 0) that the wave loop drains one wave at a time.
///
/// PERF (#740): each wave used to read its band again, pruned to the wave's
/// partitions. Row-group bboxes overlap several waves' partitions, so the
/// finest level read its band 2.5-3.4 times on NZ parcels (z14, 6 waves)
/// and 2.4-2.5 times on Moldova at width 8, decoding and clipping every
/// re-read row again. Holding the read's members in RAM instead would keep
/// up to 5.6 waves resident (measured on NZ z14) against the wave sizing's
/// one, so the store spills unless told otherwise.
///
/// Byte-identical to the per-wave reads: a member's clipped geometry does
/// not depend on the key window it was clipped in, `seq` is the same global
/// source row, the gap is measured within the row group, and
/// `encode_members` sorts by `(key, seq)`.
pub(super) fn fill_level_store(
    ctx: &LevelCtx<'_>,
    plan: &LevelPlan,
    backing: StoreBacking,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<MemberStore, ExportError> {
    // Marked before any Rayon work: a held encode that waits for it must
    // not hold the only worker the fill needs.
    #[cfg(test)]
    ctx.work
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .fills_started
        .insert(ctx.level_idx);
    let t_collect = Instant::now();
    let mut store = MemberStore::new(
        std::slice::from_ref(plan),
        backing.backing,
        backing.ram_budget,
        ctx.opts.spill_dir.as_deref(),
    )?;
    store.resident = backing.resident;
    let rows_read = collect_into(
        ctx,
        &plan.partitions,
        &mut LevelStoreSink {
            store: &mut store,
            plan,
        },
        diagnostics,
    )?;
    store.finish_fill()?;
    super::super::stream::add_nanos(&ctx.timers.spill_write, store.flush_time);
    #[cfg(test)]
    {
        let mut work = ctx.work.lock().unwrap_or_else(PoisonError::into_inner);
        *work.level_spill_bytes.entry(ctx.level_idx).or_default() += store.spilled_bytes();
    }
    log::debug!(target: LOG_TARGET,
        "[profile]     z{} level read ({} partitions, {} waves): rows_read={rows_read} \
         collect={:.2}s ({:?}, buffer {} MiB, resident {} MiB, spilled {} MiB)",
        ctx.zoom,
        plan.partitions.len(),
        plan.wave_count(),
        t_collect.elapsed().as_secs_f64(),
        backing.backing,
        store.buffer >> 20,
        backing.resident >> 20,
        store.spilled_bytes() >> 20,
    );
    Ok(store)
}

/// Read the band once, pruned to `wave`'s partitions, and clip and route
/// their members into `sink`. Returns the rows read.
pub(super) fn collect_into(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    sink: &mut dyn MemberSink,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<u64, ExportError> {
    debug_assert!(!wave.is_empty(), "wave must be non-empty");

    // Row-group pruning is only valid when the file's coordinates (and thus its
    // row-group bbox statistics) are lon/lat. For 3857 files the stats are in
    // meters; skip pruning (correct, just unpruned). Prune to the *union* of the
    // wave's partition regions (each side of the prime meridian on its own,
    // see [`PruneRegion`]), and of that union shifted ±360°: the export
    // draws a feature stored one world width away at its wrapped position
    // (#342, `wrap_shifts`), so its row group's stats sit there, not over the
    // wave's tiles.
    let bboxes = match ctx.crs {
        Crs::Epsg4326 => {
            let mut region = wave[0].bbox;
            for p in &wave[1..] {
                region.expand(&p.bbox);
            }
            // #803: with variable depth the wave's tiles can be few and far
            // apart (the rest stopped), so prune to each tile, not their union.
            let regions: Vec<PruneRegion> = match ctx.tiles {
                Some(tiles) => {
                    let margin = 360.0 / 2f64.powi(i32::from(ctx.zoom));
                    tiles
                        .range(wave[0].key_lo..=wave[wave.len() - 1].key_hi)
                        .map(|(&key, _)| {
                            let (x, y) = key_to_xy(key, ctx.zoom);
                            let mut r = PruneRegion::of(TileCoord::new(x, y, ctx.zoom).bounds());
                            r.grow(margin);
                            r
                        })
                        .collect()
                }
                None => vec![region],
            };
            Some(
                regions
                    .iter()
                    .flat_map(PruneRegion::rects)
                    .flat_map(|b| {
                        [0.0, -360.0, 360.0].map(|shift| {
                            [b.lng_min + shift, b.lat_min, b.lng_max + shift, b.lat_max]
                        })
                    })
                    .collect::<Vec<_>>(),
            )
        }
        Crs::Epsg3857 => None,
    };
    // #535: `band_read` is the reader open (footer/page-index setup, paid
    // per wave on this path) plus each `next()` (read + Arrow decode);
    // `clip_group` and `finish_group` charge their own `decode`/`clip`.
    let t_collect = crate::tile_trace::mark();
    let mut tr = CollectTrace::default();

    // PERF (#535, #817): the read runs on reader threads ahead of the clip
    // ([`read_band`]), so the pool never waits for a read-then-clip loop.
    // Output is unchanged: the batches arrive in read order, so `seq` — the
    // band-order counter the within-tile ordering derives from — advances
    // exactly as it did serially.
    // #745: the same row groups the read below selects, so the cursor knows
    // where each one starts and which band it belongs to.
    let selected = ctx
        .reader
        .selected_row_groups_any(ctx.level_idx, bboxes.as_deref())?;
    let mut rows = WaveRows {
        read: 0,
        gaps: RowGapCursor::new(ctx.reader.row_group_bands(&selected))
            .measuring(valve_merges(ctx.opts)),
        locator: ctx.reader.row_locator(&selected),
    };
    let ahead = clip_ahead_groups(ctx.band.profile);
    if ahead > 2 {
        collect_ahead(
            ctx,
            wave,
            (&selected, ahead),
            &mut rows,
            sink,
            &mut tr,
            diagnostics,
        )?;
    } else {
        collect_joined(ctx, wave, &selected, &mut rows, sink, &mut tr, diagnostics)?;
    }
    crate::tile_trace::span("collect", t_collect, || {
        serde_json::json!({
            "level": ctx.level_idx,
            "zoom": ctx.zoom,
            "groups": tr.groups,
            "ahead": tr.ahead,
            "inline": tr.inline,
            "recv_ns": tr.recv_ns,
            "join_ns": tr.join_ns,
            "inject_ns": tr.inject_ns,
            "finish_ns": tr.finish_ns,
            "clip_ns": tr.clip_ns,
            "clip_core_ns": tr.clip_core_ns,
            "serial_ns": tr.serial_ns,
            "finish_gaps_ns": tr.finish.gaps_ns,
            "finish_chunks_ns": tr.finish.chunks_ns,
            "finish_chunks_core_ns": tr.finish.chunks_core_ns,
            "finish_append_ns": tr.finish.append_ns,
        })
    });
    #[cfg(test)]
    {
        let mut work = ctx.work.lock().unwrap_or_else(PoisonError::into_inner);
        *work.band_rows_read.entry(ctx.level_idx).or_default() += rows.read;
    }
    Ok(rows.read)
}

/// [`collect_into`]'s groups, one at a time: a group's ordered finish
/// (the gap walk, member routing and the sink appends) runs alongside the
/// next group's decode and clip in one `rayon::join`.
fn collect_joined(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    selected: &[usize],
    rows: &mut WaveRows,
    sink: &mut dyn MemberSink,
    tr: &mut CollectTrace,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ExportError> {
    // PERF (#817): the overlap keeps the pool busy while this thread walks
    // the rows.
    let mut clipped: Option<ClippedGroup> = None;
    // `bounded` finishes each group before clipping the next, so it never
    // holds two groups' geometry and members: that overlap is not budgeted.
    let overlap = !matches!(ctx.band.profile, MemoryProfile::Bounded);
    let mut step = |batches: Vec<RecordBatch>| -> Result<(), ExportError> {
        if !overlap {
            if let Some(c) = clipped.take() {
                let t = crate::tile_trace::mark();
                finish_group(ctx, wave, c, rows, sink, &mut tr.finish, diagnostics)?;
                tr.serial_ns += crate::tile_trace::since(t);
            }
        }
        #[cfg(test)]
        if clipped.is_some() {
            ctx.work
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .collect_overlaps += 1;
        }
        let t_join = crate::tile_trace::mark();
        let ((t_finish, finish_ns, finished), (clip_ns, next)) =
            crate::tile_trace::wait("fill_clip", |site| {
                let joined = rayon::join(
                    || {
                        let t = crate::tile_trace::mark();
                        let r = clipped.take().map_or(Ok(()), |c| {
                            finish_group(ctx, wave, c, rows, sink, &mut tr.finish, diagnostics)
                        });
                        (t, crate::tile_trace::since(t), r)
                    },
                    || {
                        let t = crate::tile_trace::mark();
                        let r = clip_group(ctx, wave, batches, site, diagnostics);
                        (crate::tile_trace::since(t), r)
                    },
                );
                tr.clip_core_ns += site.core_ns();
                joined
            });
        if let (Some(t0), Some(t1)) = (t_join, t_finish) {
            tr.groups += 1;
            tr.join_ns += crate::tile_trace::since(t_join);
            // The join's first half starts when a worker takes the injected join.
            tr.inject_ns += t1.saturating_sub(t0);
            tr.finish_ns += finish_ns;
            tr.clip_ns += clip_ns;
        }
        finished?;
        clipped = Some(next?);
        Ok(())
    };
    let group = CollectGroup::new(collect_group_rows(ctx.band.profile));
    read_groups(ctx, selected, group, &mut tr.recv_ns, &mut step)?;
    if let Some(c) = clipped.take() {
        let t = crate::tile_trace::mark();
        finish_group(ctx, wave, c, rows, sink, &mut tr.finish, diagnostics)?;
        tr.serial_ns += crate::tile_trace::since(t);
    }
    Ok(())
}

/// [`collect_into`]'s groups, up to `ahead` held at once (#876): each read
/// batch is a group, clipped in the pool while earlier ones finish, and
/// finished strictly in read order ([`ordered_ahead`]).
///
/// PERF (#876): one group at a time held a 40-thread fill to 5–12 busy
/// threads: each `join` waited for both halves, and a group's clip, a few
/// fork-joins over ~20,000 rows, ran at only 3–11× parallelism.
///
/// Memory: `ahead` ([`clip_ahead_groups`]) single-batch groups hold the
/// batches, at most, that two of [`collect_joined`]'s groups do, under the
/// same byte cap twice, so the rows held, and their members, are unchanged.
fn collect_ahead(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    (selected, ahead): (&[usize], usize),
    rows: &mut WaveRows,
    sink: &mut dyn MemberSink,
    tr: &mut CollectTrace,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ExportError> {
    let group = CollectGroup::new(1);
    let max_bytes = group
        .max_bytes
        .saturating_mul(clip_ahead_override().map_or(2, |_| ahead));
    let clip_ns = crate::tile_trace::Tally::<1>::new();
    // One site for the whole read: its bodies are every group's clip.
    let stats = crate::tile_trace::wait("fill_clip", |site| {
        let clip = |batches: Vec<RecordBatch>| {
            let t = crate::tile_trace::mark();
            let r = clip_group(ctx, wave, batches, site, diagnostics);
            clip_ns.add(0, crate::tile_trace::since(t));
            r
        };
        let stats = super::super::pipe::ordered_ahead(
            (ahead, max_bytes),
            &clip,
            |push| {
                read_groups(ctx, selected, group, &mut tr.recv_ns, &mut |batches| {
                    let bytes = batches.iter().map(RecordBatch::get_array_memory_size).sum();
                    push(batches, bytes)
                })
            },
            |c| {
                let t = crate::tile_trace::mark();
                let r = finish_group(ctx, wave, c, rows, sink, &mut tr.finish, diagnostics);
                tr.serial_ns += crate::tile_trace::since(t);
                r
            },
        );
        tr.clip_core_ns += site.core_ns();
        stats
    })?;
    tr.groups = stats.items;
    tr.ahead = ahead;
    tr.inline = stats.inline;
    tr.join_ns = stats.wait_ns;
    tr.clip_ns = clip_ns.get(0);
    // Every finish ran here, outside a join: `serial_ns`, and the finish half.
    tr.finish_ns = tr.serial_ns;
    #[cfg(test)]
    {
        let mut work = ctx.work.lock().unwrap_or_else(PoisonError::into_inner);
        work.collect_max_held = work.collect_max_held.max(stats.max_held);
    }
    Ok(())
}

/// Read `selected` of the band ([`read_band`]) into `group`, handing each
/// full group, then the last one, to `step`. `recv_ns`: the time between
/// batches, this thread blocked on the pipe (#876).
fn read_groups(
    ctx: &LevelCtx<'_>,
    selected: &[usize],
    mut group: CollectGroup,
    recv_ns: &mut u64,
    step: &mut impl FnMut(Vec<RecordBatch>) -> Result<(), ExportError>,
) -> Result<(), ExportError> {
    let mut last = crate::tile_trace::mark();
    read_band(
        ctx.band,
        selected,
        ctx.timers,
        #[cfg(test)]
        ctx.work,
        |batch| {
            *recv_ns += crate::tile_trace::since(last);
            let bytes = batch.get_array_memory_size();
            let r = group.push(batch, bytes, &mut *step);
            last = crate::tile_trace::mark();
            r
        },
    )?;
    *recv_ns += crate::tile_trace::since(last);
    if group.batches.is_empty() {
        Ok(())
    } else {
        step(group.batches)
    }
}

/// The groups [`collect_into`] holds at once (#876), the one it is
/// finishing among them: `bounded` 1, otherwise as many read batches as
/// two of [`collect_group_rows`]'s groups take, the overlap of
/// [`collect_joined`] that the clip-ahead ([`collect_ahead`]) reuses. Two
/// or fewer (16 threads or fewer) keep the joined path.
pub(super) fn clip_ahead_groups(profile: MemoryProfile) -> usize {
    if let Some(k) = clip_ahead_override() {
        return k;
    }
    match profile {
        MemoryProfile::Bounded => 1,
        MemoryProfile::Auto | MemoryProfile::Speed => collect_group_rows(profile)
            .div_ceil(EXPORT_BATCH_SIZE)
            .saturating_mul(2),
    }
}

/// shortcut: prototype-only knob for the #876 K sweep. `TYLERTOO_CLIP_AHEAD=<K>`
/// replaces [`clip_ahead_groups`] (whatever the profile) and lifts the byte
/// cap to K batches, so the item cap binds; remove before landing.
fn clip_ahead_override() -> Option<usize> {
    std::env::var("TYLERTOO_CLIP_AHEAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|&k| k > 0)
}

/// #876: where a traced [`collect_into`]'s driving thread spent its wall,
/// for its `collect` trace event. Untouched untraced.
#[derive(Default)]
struct CollectTrace {
    /// Finish-and-clip joins, or groups the clip-ahead took.
    groups: u64,
    /// [`clip_ahead_groups`] when the read clipped ahead, else 0; and the
    /// groups it clipped on this thread, which no worker had started.
    ahead: usize,
    inline: u64,
    /// Blocked on the reader pipe.
    recv_ns: u64,
    /// In the joins, and of that, before a worker started the injected join.
    /// Clipping ahead: waiting for a group's clip, run here or on a worker.
    join_ns: u64,
    inject_ns: u64,
    /// Inside the joins: `finish_group`'s and `clip_group`'s wall, and the
    /// clip's closure bodies' core time on every thread.
    finish_ns: u64,
    clip_ns: u64,
    clip_core_ns: u64,
    /// `finish_group` on this thread, outside a join.
    serial_ns: u64,
    /// Every `finish_group`'s stages.
    finish: FinishTrace,
}

/// #876: where a traced [`finish_group`]'s wall went: the ordered gap
/// walk, the parallel member chunks (wall, and their bodies' core time),
/// and the ordered appends to the sink.
#[derive(Default)]
pub(super) struct FinishTrace {
    gaps_ns: u64,
    chunks_ns: u64,
    chunks_core_ns: u64,
    append_ns: u64,
}

/// The batches [`collect_into`] gathers before one [`clip_group`]: up to
/// `max_rows` rows, and never past one reader batch's decoded-byte cap
/// ([`tylertoo_io::decoded_batch_limit`]) unless a single batch is that
/// large on its own, so grouping holds no more than the reader allows.
pub(super) struct CollectGroup {
    pub(super) batches: Vec<RecordBatch>,
    pub(super) rows: usize,
    pub(super) bytes: usize,
    pub(super) max_rows: usize,
    pub(super) max_bytes: usize,
}

impl CollectGroup {
    pub(super) fn new(max_rows: usize) -> Self {
        Self {
            batches: Vec::new(),
            rows: 0,
            bytes: 0,
            max_rows,
            max_bytes: usize::try_from(tylertoo_io::decoded_batch_limit()).unwrap_or(usize::MAX),
        }
    }

    /// Add `batch` (`bytes` decoded), handing each full group to `flush`:
    /// the gathered batches first when `batch` would take them past the
    /// byte cap, then the group once it reaches either cap.
    pub(super) fn push(
        &mut self,
        batch: RecordBatch,
        bytes: usize,
        flush: &mut impl FnMut(Vec<RecordBatch>) -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        if !self.batches.is_empty() && self.bytes.saturating_add(bytes) > self.max_bytes {
            self.flush(flush)?;
        }
        self.rows += batch.num_rows();
        self.bytes = self.bytes.saturating_add(bytes);
        self.batches.push(batch);
        if self.rows >= self.max_rows || self.bytes >= self.max_bytes {
            self.flush(flush)?;
        }
        Ok(())
    }

    pub(super) fn flush(
        &mut self,
        flush: &mut impl FnMut(Vec<RecordBatch>) -> Result<(), ExportError>,
    ) -> Result<(), ExportError> {
        self.rows = 0;
        self.bytes = 0;
        flush(std::mem::take(&mut self.batches))
    }
}

/// How the export reads a band: the overview file as a [`ConvertSource`],
/// so every band read goes through the ordered multi-worker reader
/// ([`read_in_order`]), and the budget and profile that size its read-ahead.
#[derive(Clone, Copy)]
pub(super) struct BandRead<'a> {
    pub(super) source: &'a ConvertSource,
    pub(super) budget: MemoryBudget,
    pub(super) profile: MemoryProfile,
}

/// Read the row groups `selected` of the overview file in file order, in
/// [`EXPORT_BATCH_SIZE`] batches, and hand each to `on_batch` on this
/// thread. Up to [`resolve_pass_read_workers`] readers decode disjoint runs
/// of row groups ahead of the consumer, and the merge puts their batches
/// back in read order, so `on_batch` sees the rows exactly as one reader
/// would hand them over. `band_read` is charged the readers' core-seconds.
///
/// PERF (#817): several readers, as one reader thread cannot decompress
/// pages as fast as the pool clips them.
pub(super) fn read_band(
    band: BandRead<'_>,
    selected: &[usize],
    timers: &ExportTimers,
    #[cfg(test)] work: &Mutex<ExportWorkTally>,
    mut on_batch: impl FnMut(RecordBatch) -> Result<(), ExportError>,
) -> Result<(), ExportError> {
    // The collect takes a group at a time, so each reader needs about one
    // group queued, not the budget's depth: concurrent levels each read.
    let ahead = collect_group_rows(band.profile).div_ceil(EXPORT_BATCH_SIZE);
    #[cfg(test)]
    let consumer = std::thread::current().id();
    // The read runs on its own thread: with one reader (few cores, a tight
    // budget, one segment) `read_in_order` reads on its caller's thread,
    // and the pool would wait on each read.
    scoped_pipe(
        SINGLE_READ_IN_FLIGHT,
        |tx: &Sender<RecordBatch>| -> Result<(), ExportError> {
            read_overview(
                band,
                selected.to_vec(),
                None,
                ReadPass::Build,
                ahead,
                |batch, read| {
                    add_nanos(&timers.band_read, read);
                    #[cfg(test)]
                    if std::thread::current().id() == consumer {
                        work.lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .band_reads_on_consumer += 1;
                    }
                    // A closed channel is the consumer's error: stop cleanly.
                    Ok(if tx.send(batch).is_ok() {
                        ReadFlow::Continue
                    } else {
                        ReadFlow::Stop
                    })
                },
            )
        },
        |rx| rx.into_iter().try_for_each(&mut on_batch),
    )
}

/// Read the row groups `selected` of the overview in file order through
/// the ordered reader ([`read_in_order_capped`]), projected to `roots`
/// (`None`: every column), each reader at most `ahead` batches ahead, and
/// hand each batch and its read time to `on_batch` on this thread. Every
/// read of the overview file the export makes goes through here (#833).
pub(super) fn read_overview(
    band: BandRead<'_>,
    selected: Vec<usize>,
    roots: Option<&[usize]>,
    pass: ReadPass,
    ahead: usize,
    mut on_batch: impl FnMut(RecordBatch, Duration) -> Result<ReadFlow, ExportError>,
) -> Result<(), ExportError> {
    let selection = RowGroupSelection::from_parts(vec![selected]);
    let tuning = band_tuning(band, &selection, roots, pass)?;
    let mut failed = None;
    read_in_order_capped(
        band.source,
        Some(&selection),
        roots,
        tuning,
        Some(ahead),
        |batch, _, read| {
            on_batch(batch, read).or_else(|e| {
                failed = Some(e);
                Ok(ReadFlow::Stop)
            })
        },
    )
    .map_err(read_error)?;
    failed.map_or(Ok(()), Err)
}

/// Read every band of the overview, in band order, through
/// [`read_overview`] and send each batch down `tx` tagged with its band:
/// the scan's and the single-read fill's producer. Stops cleanly when the
/// consumer hangs up.
pub(super) fn send_bands(
    (reader, band): (&OverviewReader, BandRead<'_>),
    roots: Option<&[usize]>,
    pass: ReadPass,
    mut charge_read: impl FnMut(Duration),
    tx: &Sender<(usize, RecordBatch)>,
    #[cfg(test)] readers: &mut usize,
) -> Result<(), ExportError> {
    for j in 0..reader.num_levels() {
        let mut hung_up = false;
        read_overview(
            band,
            reader.band(j)?,
            roots,
            pass,
            BAND_READ_AHEAD_BATCHES,
            |batch, read| {
                charge_read(read);
                hung_up = tx.send((j, batch)).is_err();
                Ok(if hung_up {
                    ReadFlow::Stop
                } else {
                    ReadFlow::Continue
                })
            },
        )?;
        #[cfg(test)]
        {
            *readers = (*readers).max(super::super::read::last_read_workers());
        }
        if hung_up {
            return Ok(()); // consumer dropped the receiver
        }
    }
    Ok(())
}

/// How [`read_in_order_capped`] reads `selection` of the overview for
/// `pass`, over the `projection` columns (`None`: all of them).
pub(super) fn band_tuning(
    band: BandRead<'_>,
    selection: &RowGroupSelection,
    projection: Option<&[usize]>,
    pass: ReadPass,
) -> Result<ReadTuning, ExportError> {
    let footer = match projection {
        Some(p) => footer_price(band.source, Some(selection), p.iter().copied()),
        None => {
            let columns = band
                .source
                .schema()
                .map_err(|e| read_error(e.into()))?
                .fields()
                .len();
            footer_price(band.source, Some(selection), 0..columns)
        }
    }
    .map_err(read_error)?;
    Ok(ReadTuning {
        budget: band.budget,
        batch_size: EXPORT_BATCH_SIZE,
        // `bounded` reads with one worker: concurrent level reads each size
        // their read-ahead from the same budget, uncharged.
        workers: match band.profile {
            MemoryProfile::Bounded => 1,
            MemoryProfile::Auto | MemoryProfile::Speed => {
                resolve_pass_read_workers(pass, READ_WORKERS_AUTO)
            }
        },
        avg_geom_bytes: None,
        footer,
        profile: band.profile,
    })
}

/// A band read's [`ConvertError`] as the export's error, keeping its typed
/// cause: [`Classify`](crate::error::Classify) finds a hostile input in the
/// source chain.
pub(super) fn read_error(e: ConvertError) -> ExportError {
    match e {
        ConvertError::Parquet(e) | ConvertError::Input(InputError::Parquet(e)) => {
            ExportError::Reader(ReaderError::Parquet(e))
        }
        ConvertError::Io(e) | ConvertError::Input(InputError::Io(e)) => ExportError::Io(e),
        ConvertError::Core(e) => ExportError::Core(e),
        ConvertError::Arrow(e) | ConvertError::Input(InputError::Arrow(e)) => ExportError::Arrow(e),
        ConvertError::Input(InputError::Decode(e)) => {
            ExportError::Arrow(arrow_schema::ArrowError::ExternalError(Box::new(e)))
        }
        e => ExportError::Arrow(arrow_schema::ArrowError::ExternalError(Box::new(e))),
    }
}

/// Rows per pool thread that [`collect_into`] gathers, in whole batches,
/// before one [`clip_group`] and [`finish_group`]: 64 threads collect four
/// 8,192-row batches at a time, 16 threads one. Each group holds its
/// batches and their members until it is merged.
pub(super) const COLLECT_GROUP_ROWS_PER_THREAD: usize = 512;

/// The rows [`collect_into`] gathers per group on this pool. `bounded`
/// takes each read batch on its own: the groups' decoded and clipped rows
/// are not priced in the budget.
pub(super) fn collect_group_rows(profile: MemoryProfile) -> usize {
    match profile {
        MemoryProfile::Bounded => 1,
        MemoryProfile::Auto | MemoryProfile::Speed => {
            COLLECT_GROUP_ROWS_PER_THREAD.saturating_mul(rayon::current_num_threads())
        }
    }
}

/// The wave partition owning `key`. The wave's partitions are contiguous,
/// disjoint, ascending `[key_lo, key_hi]` ranges, and every emitted key was
/// scanned (so it lies in exactly one of them), giving an unambiguous home.
#[inline]
pub(super) fn route_partition(wave: &[Partition], key: u64) -> usize {
    // Rightmost partition whose `key_lo <= key`.
    let idx = wave.partition_point(|p| p.key_lo <= key).saturating_sub(1);
    debug_assert!(
        key >= wave[idx].key_lo && key <= wave[idx].key_hi,
        "member key {key:x} falls outside its wave partition [{:x}..{:x}]",
        wave[idx].key_lo,
        wave[idx].key_hi,
    );
    idx
}

/// Rows per parallel chunk of a batch's per-row collect work (#740): one
/// chunk per pool thread, but at least [`MIN_COLLECT_CHUNK_ROWS`] rows, so the
/// per-chunk setup (column slices, buckets) stays small next to the rows. The
/// chunking never shows in the output: every chunk's results are put back in
/// row order.
pub(super) fn collect_chunk_rows(rows: usize) -> usize {
    rows.div_ceil(rayon::current_num_threads())
        .max(MIN_COLLECT_CHUNK_ROWS)
        .min(rows.max(1))
}

/// The smallest [`collect_chunk_rows`] chunk of a batch larger than it. One
/// row under test, so the small test batches still split into chunks at
/// nonzero offsets and every byte-identity test covers the merge.
pub(super) const MIN_COLLECT_CHUNK_ROWS: usize = if cfg!(test) { 1 } else { 128 };

/// The row ranges [`collect_chunk_rows`] splits a batch of `rows` into, in
/// row order.
pub(super) fn collect_chunks(
    rows: usize,
) -> impl IndexedParallelIterator<Item = std::ops::Range<usize>> {
    let chunk = collect_chunk_rows(rows);
    (0..rows.div_ceil(chunk))
        .into_par_iter()
        .map(move |i| i * chunk..((i + 1) * chunk).min(rows))
}

/// Decode a batch's geometry column to `geo::Geometry` (geoarrow, no WKB
/// round trip), reprojecting EPSG:3857 input to lon/lat — shared by both
/// pass-2 paths. #535: charged to `decode`, per chunk (core-seconds).
///
/// PERF (#740): the decode runs over row chunks in the pool. It used to be
/// one serial pass on the wave's consumer thread, about 28% of that
/// thread's time on a 64-core Moldova export, while the pool waited for it.
/// Each chunk decodes a zero-copy slice of the column, and the chunks are
/// concatenated in row order, so the result is the serial one.
pub(super) fn decode_batch_geometries(
    batch: &RecordBatch,
    schema: &Schema,
    geom_idx: usize,
    crs: Crs,
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<Geometry<f64>>, ExportError> {
    let geom_field = schema.field(geom_idx).clone();
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::GeoParquetReadError::decode("geometry decode", e))?;
    let chunks: Vec<Vec<Geometry<f64>>> = collect_chunks(batch.num_rows())
        .map(|range| {
            ExportTimers::time(&timers.decode, || {
                decode_geometry_chunk(garr.as_ref(), range, crs, diagnostics)
            })
        })
        .collect::<Result<_, _>>()?;
    Ok(chunks.into_iter().flatten().collect())
}

/// One chunk of [`decode_batch_geometries`]: rows `range` of `garr`.
pub(super) fn decode_geometry_chunk(
    garr: &dyn GeoArrowArray,
    range: std::ops::Range<usize>,
    crs: Crs,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<Geometry<f64>>, ExportError> {
    let slice = garr.slice(range.start, range.len());
    let mut rows = Vec::with_capacity(range.len());
    crate::batch_processor::extract_geometries_opt_from_array(
        slice.as_ref(),
        &mut rows,
        diagnostics,
    )?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let g = row.unwrap_or_else(|| {
                Geometry::GeometryCollection(geo::GeometryCollection::default())
            });
            match crs {
                Crs::Epsg3857 => reproject_3857_to_4326(&g),
                Crs::Epsg4326 => g,
            }
        })
        .collect())
}

/// The finest level's zoom and junction pins (#802), at which
/// [`finest_zoom_geometry`] thins; `None` at every other level.
pub(super) type Finest<'p> = Option<(u8, Option<&'p JunctionNodes>)>;

/// #802: the junction pins of the finest level's polygons, which
/// [`finest_zoom_geometry`] keeps so that neighbours thin a shared border
/// alike. Convert's pins key the source's coordinates, which export may see
/// reprojected, so they are rebuilt here from the level itself. `None`
/// unless max-zoom simplification thins polygons, or when the pins outgrow
/// the budget (which records [`Diagnostic::SharedBordersUnpinned`]); the
/// pins are then charged to the budget.
///
/// [`Diagnostic::SharedBordersUnpinned`]: crate::diagnostics::Diagnostic::SharedBordersUnpinned
pub(super) fn finest_level_pins(
    (reader, band): (&OverviewReader, BandRead<'_>),
    crs: Crs,
    opts: &ExportOptions,
    budget: &mut MemoryBudget,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Option<JunctionNodes>, ExportError> {
    let (Some(geometry), true) = (
        reader.resolved_schema().geometry_index(),
        opts.shared_nodes && opts.max_zoom_simplify_factor.is_some_and(|f| f > 0.0),
    ) else {
        return Ok(None);
    };
    let selected = reader.row_groups_for_level(reader.num_levels() - 1)?;
    let timers = ExportTimers::default();
    let (mut occ, mut fits) = (Vec::new(), true);
    read_overview(
        band,
        selected,
        Some(&[geometry]),
        ReadPass::Scan,
        BAND_READ_AHEAD_BATCHES,
        |batch, _| {
            let geoms =
                decode_batch_geometries(&batch, &batch.schema(), 0, crs, &timers, diagnostics)?;
            let mut chunks = junction::occurrences_of(&geoms);
            let fit = |bytes| budget.require(MemoryPhase::ExportCollect, bytes).is_ok();
            fits = junction::absorb(&mut occ, &mut chunks, fit, diagnostics);
            Ok(if fits {
                ReadFlow::Continue
            } else {
                ReadFlow::Stop
            })
        },
    )?;
    let pins = fits
        .then(|| JunctionNodes::from_occurrences(occ))
        .filter(|p| !p.keys().is_empty());
    if let Some(p) = &pins {
        budget.reserve_up_to(MemoryPhase::ExportCollect, p.resident_bytes());
    }
    Ok(pins)
}

/// A feature of the finest level, simplified by
/// [`ExportOptions::max_zoom_simplify_factor`] (#814): `factor × gsd(z)` at
/// that level's zoom (`finest`), the tolerance `--simplify-factor` gives
/// every coarser level. Unset, only lines are simplified, at
/// [`DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR`]. Borrowed unchanged at other
/// levels (`None`) and at factor 0.
///
/// DIVERGENCE FROM TIPPECANOE: tippecanoe's `-S` simplifies each tile's
/// clipped geometry, at every zoom, in tile units (`tile.cpp`,
/// `simplify_lines`). Coarser levels here are already simplified by convert,
/// so this touches only the finest one, and simplifies each feature once,
/// before clipping, so neighbouring tiles agree at their seam. It only
/// removes vertices: a feature, part or ring under the tolerance keeps its
/// own geometry rather than being dropped, and a polygon with no valid
/// candidate keeps its own (no repair), or, under pins, the candidate, which
/// the tile-space repair resolves.
/// Coarser levels keep `simplify_for_level`'s rules.
///
/// DIVERGENCE FROM TIPPECANOE: tippecanoe's default `-S 1` simplifies
/// polygons at the max zoom too. The default here leaves them alone, so
/// the finest zoom draws the source as is; `--max-zoom-simplify-factor 0.25`
/// or `--preset tippecanoe` simplifies both, polygons cut at `finest`'s
/// junction pins ([`finest_level_pins`], #802).
pub(super) fn finest_zoom_geometry<'g>(
    geom: &'g Geometry<f64>,
    finest: Finest<'_>,
    opts: &ExportOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> std::borrow::Cow<'g, Geometry<f64>> {
    use super::super::simplify::{
        level_tolerance, simplify_line_vertices_only, simplify_polygon_vertices_only,
    };
    let line = matches!(geom, Geometry::LineString(_) | Geometry::MultiLineString(_));
    let factor = match opts.max_zoom_simplify_factor {
        Some(factor) => factor,
        None if line => DEFAULT_LINE_MAX_ZOOM_SIMPLIFY_FACTOR,
        None => 0.0,
    };
    let Some((zoom, pins)) = finest.filter(|_| factor > 0.0) else {
        return std::borrow::Cow::Borrowed(geom);
    };
    // Export geometry is lon/lat whatever the file's CRS (`decode_geometry_chunk`).
    let tol = level_tolerance(super::super::level::gsd(zoom), Crs::Epsg4326, factor);
    // Part by part and ring by ring, so nothing but vertices goes: a part or
    // ring under the tolerance keeps its own geometry, as a whole feature
    // does, since at the finest level it can still span tile units.
    let line = |ls| simplify_line_vertices_only(ls, tol);
    let polygon = |p| simplify_polygon_vertices_only(p, tol, pins, diagnostics);
    let out = match geom {
        Geometry::LineString(ls) => Geometry::LineString(line(ls)),
        Geometry::MultiLineString(m) => {
            Geometry::MultiLineString(geo::MultiLineString::new(m.0.iter().map(line).collect()))
        }
        Geometry::Polygon(p) => Geometry::Polygon(polygon(p)),
        Geometry::MultiPolygon(m) => {
            Geometry::MultiPolygon(geo::MultiPolygon::new(m.0.iter().map(polygon).collect()))
        }
        // Points have nothing to simplify; a collection keeps its members.
        _ => return std::borrow::Cow::Borrowed(geom),
    };
    std::borrow::Cow::Owned(out)
}

/// One feature's clipped pieces, one per tile key.
pub(super) type TileMembers = Vec<(u64, MemberGeom)>;

/// A group of consecutive read batches, decoded and clipped by
/// [`clip_group`], waiting for [`finish_group`] to append their members to
/// the wave's per-partition buckets. Together the two clip every feature (in
/// parallel) into its intersecting tiles across the wave's whole key range,
/// route each member to its owning partition, then attach shared per-feature
/// properties in band order. `rows` restores the original overview addresses after pruning,
/// keeping identity stable between waves while preserving the relative
/// within-tile feature order.
///
/// PERF (#740): property extraction, member materialization and routing run
/// over row chunks in the pool ([`collect_member_chunk`]); only appending
/// each chunk's buckets, in chunk order, stays serial. The chunks are
/// merged in row order, so every bucket holds the same members in the same
/// order as the serial loop gave.
///
/// PERF (#817): the batches of a group ([`COLLECT_GROUP_ROWS_PER_THREAD`])
/// share each parallel step, so a 64-thread pool joins once per group, not
/// once per 8,192-row batch, whose joins would leave the pool spinning.
/// Batches and their chunks are still merged in row order.
pub(super) struct ClippedGroup {
    pub(super) batches: Vec<RecordBatch>,
    pub(super) geom_idxs: Vec<usize>,
    /// The decoded geometries' projected bboxes, kept for the in-order gap
    /// walk; the geometries themselves went to their members.
    pub(super) boxes: Vec<Vec<Option<super::coalesce_valve::RowBox>>>,
    /// Per batch, per row: the row's `(key, clipped geometry)` members.
    pub(super) row_members: Vec<Vec<TileMembers>>,
}

/// The order-free half of a group's collect: decode a group's
/// geometries and clip every feature into its tiles, all in the pool.
pub(super) fn clip_group(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    batches: Vec<RecordBatch>,
    site: &crate::tile_trace::Site,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<ClippedGroup, ExportError> {
    let key_lo = wave[0].key_lo;
    let key_hi = wave[wave.len() - 1].key_hi;
    let timers = ctx.timers;
    let geom_idxs: Vec<usize> = batches
        .iter()
        .map(|b| batch_geometry_index(b, ctx.published.geometry.as_deref()))
        .collect::<Result<_, _>>()?;
    let geoms: Vec<Vec<Geometry<f64>>> = batches
        .par_iter()
        .zip(&geom_idxs)
        .map(|(b, &gi)| {
            site.body(|| decode_batch_geometries(b, &b.schema(), gi, ctx.crs, timers, diagnostics))
        })
        .collect::<Result<_, _>>()?;
    // The canonical level's own zoom, not `ctx.zoom`: a variable-depth probe
    // clips canonical geometry at an ancestor's zoom (#803).
    let finest_zoom = (ctx.level_idx + 1 == ctx.reader.num_levels())
        .then(|| zoom_for_level(ctx.reader.meta(), ctx.level_idx).map(|z| (z, ctx.pins)))
        .transpose()?;
    // Parallel per-feature clip (H3(c) lever 2), over the wave's whole key
    // range. #535: `clip` is timed per geometry inside the closure.
    let measure = valve_merges(ctx.opts);
    let boxes = geoms
        .par_iter()
        .map(|geoms| site.body(|| RowGapCursor::boxes(measure, geoms)))
        .collect();
    let row_members = geoms
        .into_par_iter()
        .map(|geoms| {
            geoms
                .into_par_iter()
                .map(|g| {
                    // #876: a clip a tile's thread runs is foreign to the tile.
                    let zoom = || crate::tile_trace::Ctx::zoom(ctx.zoom);
                    site.body(|| {
                        crate::tile_trace::job(zoom, || {
                            ExportTimers::time(&timers.clip, || {
                                let mut members = shared_tile_members(
                                    MemberSource::Owned(g),
                                    finest_zoom,
                                    ctx.zoom,
                                    ctx.opts,
                                    (key_lo, key_hi),
                                    diagnostics,
                                );
                                if let Some(tiles) = ctx.tiles {
                                    members.retain(|(key, _)| tiles.contains_key(key));
                                }
                                members
                            })
                        })
                    })
                })
                .collect()
        })
        .collect();
    Ok(ClippedGroup {
        batches,
        geom_idxs,
        boxes,
        row_members,
    })
}

/// The ordered half of a group's collect ([`clip_group`] is the other): walk the group's rows for
/// their gaps, in read order, then materialize and route their members and
/// append them to `sink` in row order.
pub(super) fn finish_group(
    ctx: &LevelCtx<'_>,
    wave: &[Partition],
    group: ClippedGroup,
    rows: &mut WaveRows,
    sink: &mut dyn MemberSink,
    tr: &mut FinishTrace,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ExportError> {
    let timers = ctx.timers;
    let ClippedGroup {
        batches,
        geom_idxs,
        boxes,
        mut row_members,
    } = group;
    let batches = &batches;
    #[cfg(test)]
    {
        let mut work = ctx.work.lock().unwrap_or_else(PoisonError::into_inner);
        work.collect_batches += batches.len() as u64;
        work.collect_groups += 1;
    }
    let t = crate::tile_trace::mark();
    let row_gaps: Vec<Vec<(u64, u16)>> = ExportTimers::time(&timers.decode, || {
        boxes.iter().map(|b| rows.gaps.batch_boxes(b)).collect()
    });
    tr.gaps_ns += crate::tile_trace::since(t);

    // Property extraction and member routing, per chunk in the pool. #535:
    // `decode`, timed per chunk and around the merge below.
    let mut first_read = rows.read as usize;
    let attrs: Vec<AttrColumns<'_>> = batches
        .iter()
        .zip(&geom_idxs)
        .map(|(b, &gi)| {
            let published = ctx.published.at_level(ctx.level_idx);
            AttrColumns::new(&b.schema(), gi, published, ctx.feature_id)
        })
        .collect();
    let chunk_inputs: Vec<ChunkInput<'_>> = batches
        .iter()
        .zip(&attrs)
        .zip(&row_gaps)
        .map(|((batch, attrs), row_gaps)| {
            let input = ChunkInput {
                #[cfg(test)]
                work: ctx.work,
                wave,
                batch,
                attrs,
                row_gaps,
                locator: &rows.locator,
                first_read,
            };
            first_read += batch.num_rows();
            input
        })
        .collect();
    let t = crate::tile_trace::mark();
    // #876: the owner works the chunks too and then blocks, never stealing:
    // a stolen pending clip doubled each finish's wall when clipping ahead.
    // One uncontended lock per chunk hands its rows to whichever thread runs it.
    let work: Vec<(&ChunkInput<'_>, usize, std::sync::Mutex<&mut [TileMembers]>)> = row_members
        .iter_mut()
        .zip(&chunk_inputs)
        .flat_map(|(row_members, chunk)| {
            let chunk_rows = collect_chunk_rows(chunk.batch.num_rows());
            row_members
                .chunks_mut(chunk_rows)
                .enumerate()
                .map(move |(i, items)| (chunk, i * chunk_rows, std::sync::Mutex::new(items)))
        })
        .collect();
    let chunks: Vec<Result<ChunkMembers, ExportError>> =
        crate::tile_trace::wait("fill_finish", |site| {
            let chunks = crate::owner_map::owner_map(&work, |(chunk, first, items)| {
                let mut items = items.lock().unwrap_or_else(PoisonError::into_inner);
                site.body(|| {
                    ExportTimers::time(&timers.decode, || {
                        collect_member_chunk(chunk, *first, &mut items)
                    })
                })
            });
            tr.chunks_core_ns += site.core_ns();
            chunks
        });
    drop(work);
    tr.chunks_ns += crate::tile_trace::since(t);
    drop(row_members);
    let t = crate::tile_trace::mark();

    for chunk in chunks {
        let ChunkMembers {
            buckets: chunk_buckets,
            losses,
        } = chunk?;
        // Recorded here, in row order, as the serial loop recorded them.
        ExportTimers::time(&timers.decode, || {
            losses.into_iter().try_for_each(|(source_row, id)| {
                diagnostics.source_member_loss(
                    source_row,
                    id,
                    ctx.zoom,
                    crate::diagnostics::SourceLossCause::ClipDropped,
                )
            })
        })?;
        // Not `decode`: a spilling sink charges its own writes to
        // `spill_write`.
        for (partition, members) in chunk_buckets.into_iter().enumerate() {
            if !members.is_empty() {
                sink.append(partition, members)?;
            }
        }
    }
    tr.append_ns += crate::tile_trace::since(t);
    rows.read = first_read as u64;
    Ok(())
}

/// Where [`finish_group`] puts a read's members: one call per
/// partition (an index into the partitions being read) and row chunk, in row
/// order. The per-wave read collects into one bucket per partition; a
/// multi-wave level's single read (#740) into a [`MemberStore`], whose waves
/// the drain then takes, or replays from spill, one at a time.
pub(super) trait MemberSink: Send {
    fn append(&mut self, partition: usize, members: Vec<Member>) -> Result<(), ExportError>;
}

impl MemberSink for Vec<Vec<Member>> {
    fn append(&mut self, partition: usize, mut members: Vec<Member>) -> Result<(), ExportError> {
        let bucket = &mut self[partition];
        if bucket.is_empty() {
            *bucket = members;
        } else {
            bucket.append(&mut members);
        }
        Ok(())
    }
}

/// A one-level [`MemberStore`] as a [`MemberSink`] over all of the level's
/// partitions: a partition's members go to its wave's bucket.
pub(super) struct LevelStoreSink<'a> {
    pub(super) store: &'a mut MemberStore,
    /// The level whose waves the store's buckets are.
    pub(super) plan: &'a LevelPlan,
}

impl MemberSink for LevelStoreSink<'_> {
    fn append(&mut self, partition: usize, members: Vec<Member>) -> Result<(), ExportError> {
        let wave = self.plan.wave_of(partition);
        members
            .into_iter()
            .try_for_each(|m| self.store.push(0, wave, m))
    }
}

/// What every chunk of one batch's [`collect_member_chunk`] reads.
pub(super) struct ChunkInput<'a> {
    /// The export's test-only work counts (#788).
    #[cfg(test)]
    pub(super) work: &'a Mutex<ExportWorkTally>,
    pub(super) wave: &'a [Partition],
    pub(super) batch: &'a RecordBatch,
    pub(super) attrs: &'a AttrColumns<'a>,
    /// Every row's `(gap, band)`, from the wave's [`RowGapCursor`].
    pub(super) row_gaps: &'a [(u64, u16)],
    pub(super) locator: &'a crate::input_set::StreamRowLocator,
    /// The wave's read position at the batch's first row.
    pub(super) first_read: usize,
}

/// One chunk's members, bucketed per wave partition, and the
/// `(source row, feature id)` of each of its rows that carries a
/// `--feature-id`, in row order.
pub(super) struct ChunkMembers {
    pub(super) buckets: Vec<Vec<Member>>,
    pub(super) losses: Vec<(u64, u64)>,
}

/// Materialize the members of the rows `first..first + items.len()` of a
/// batch: each row's properties (once per row, shared via `Arc`) attached to
/// each of its clipped `(key, geometry)` items, routed to the item's wave
/// partition. Takes the items out of `items`.
pub(super) fn collect_member_chunk(
    input: &ChunkInput<'_>,
    first: usize,
    items: &mut [TileMembers],
) -> Result<ChunkMembers, ExportError> {
    #[cfg(test)]
    if rayon::current_thread_index().is_none() {
        input
            .work
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .serial_collect_rows += items.len() as u64;
    }
    let columns = input.batch.slice(first, items.len());
    let mut attrs = input.attrs.extract(&columns)?;
    let mut out = ChunkMembers {
        buckets: (0..input.wave.len()).map(|_| Vec::new()).collect(),
        losses: Vec::new(),
    };
    for (i, items) in items.iter_mut().enumerate() {
        let row = first + i;
        let source_row = input.locator.global_row(input.first_read + row).ok_or(
            crate::Error::GeoParquetRead(crate::GeoParquetReadError::RowDomainExceeded),
        )?;
        let id = attrs.id(i)?;
        if let Some(id) = id {
            out.losses.push((source_row, id));
        }
        if items.is_empty() {
            continue;
        }
        let props = attrs.props(i);
        let (gap, band) = input.row_gaps[row];
        for (key, geom) in std::mem::take(items) {
            out.buckets[route_partition(input.wave, key)].push(Member {
                key,
                seq: source_row,
                rank: attrs.rank(i, source_row),
                geom,
                props: props.clone(),
                id,
                gap,
                band,
            });
        }
    }
    Ok(out)
}

#[derive(Clone, Copy)]
pub(super) struct TileTarget<'a> {
    pub(super) zoom: u8,
    pub(super) opts: &'a ExportOptions,
    pub(super) key_lo: u64,
    pub(super) key_hi: u64,
    pub(super) assume_simple: bool,
    pub(super) diagnostics: &'a crate::diagnostics::Diagnostics,
    /// The geometry the clip started from: a piece that is still it is
    /// [`TilePiece::Whole`].
    pub(super) source: &'a Geometry<f64>,
}

/// Clip (or fast-path pass through) one feature into every tile it intersects
/// at `zoom` whose key falls within `[key_lo, key_hi]`, via a **top-down
/// recursive quadtree cascade** (issue #226, tippecanoe's tiling model).
///
/// ## Why recursion (the tiles×vertices blowup)
///
/// The straightforward loop — enumerate every tile the feature bbox covers and
/// clip the feature's **full** geometry against each — costs
/// `Σ_features (tiles_spanned × vertices)`. A large admin polygon covering a
/// fraction `F` of the map touches `≈ (F·2^z)²` tiles at zoom `z`, each a full
/// ring clip; at z12–14 this is billions of clip-vertex ops (the adm4 export
/// DNF'd at 3h13m). This is the spatial analogue of #218's repeated work.
///
/// Instead we walk the tile pyramid from the root down to `zoom`. At each node
/// we clip the parent's **already-reduced** geometry to the node's bounds plus
/// buffer, then split into four children. Because a child's buffered bounds are
/// contained in its parent's (the pixel buffer in world units doubles per level
/// up, so `child ± buf(child) ⊆ parent ± buf(parent)` on both axes, Mercator
/// latitude included), the cascade is a proper superset chain and
/// `clip(clip(G, parent), leaf) = clip(G, leaf)` — the leaf result is the same
/// clip as the direct loop would produce (modulo float-noise / ring-normalization
/// on genuine seam-crossers; interior features pass through byte-identical). Each
/// vertex now takes part in `O(depth)` clips, not `O(tiles_spanned)`.
///
/// ## Identical leaf set
///
/// The recursion is bounded by [`member_ranges`] — the same helper the scan
/// pass uses — so the emitted key set equals
/// `member_ranges(feature_bbox, zoom, opts) ∩ [key_lo, key_hi]`, minus the
/// candidates the clip rejects. That keeps the scan pass's per-tile counts
/// valid: every emitted key lies in a planned partition.
///
/// ## World-edge wrap (#342)
///
/// A feature whose bbox reaches past ±180°, or into the buffer of the tile
/// column on the far side of the antimeridian, is tiled once per
/// [`wrap_shifts`] copy: as stored, and shifted ±360°. Each copy goes through
/// the same membership and clip as an ordinary feature, so a cell stored as
/// `x: 178 … 181` keeps its 178°–180° part in the east-edge tile and draws its
/// 180°–181° part at -180°…-179° in the west-edge tile. Where two copies land
/// in one tile (z0 holds the whole world), their pieces join into one
/// multi-geometry, so a feature still emits at most one member per key.
pub(super) fn feature_tile_pieces(
    geom: &Geometry<f64>,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Vec<(u64, TilePiece)> {
    let Some(rect) = geom.bounding_rect() else {
        return Vec::new();
    };
    let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
    // Ring simplicity survives a pure x translation, so one check covers every
    // copy (issue #237, RC3).
    let assume_simple = geometry_is_simple(geom);
    let buffer = buffer_deg_at_zoom(zoom, opts);
    let mut out = Vec::new();
    if !near_world_edge(&bbox, buffer) {
        copy_tile_members(
            geom,
            &bbox,
            TileTarget {
                zoom,
                opts,
                key_lo,
                key_hi,
                assume_simple,
                diagnostics,
                source: geom,
            },
            &mut out,
        );
        return out;
    }
    // A piece of the stored copy (shift 0) that no other copy joins stays
    // whole.
    let mut by_key: BTreeMap<u64, TilePiece> = BTreeMap::new();
    for shift in wrap_shifts(&bbox, buffer) {
        let copy = shift_lng(geom, shift);
        copy_tile_members(
            &copy,
            &shift_bbox(&bbox, shift),
            TileTarget {
                zoom,
                opts,
                key_lo,
                key_hi,
                assume_simple,
                diagnostics,
                source: &copy,
            },
            &mut out,
        );
        let owned = |piece| match piece {
            TilePiece::Whole => copy.clone().into_owned(),
            TilePiece::Clipped(g) => g,
        };
        for (key, piece) in out.drain(..) {
            let piece = match by_key.remove(&key) {
                Some(TilePiece::Whole) => {
                    TilePiece::Clipped(join_wrapped_pieces(geom.clone(), owned(piece)))
                }
                Some(TilePiece::Clipped(prev)) => {
                    TilePiece::Clipped(join_wrapped_pieces(prev, owned(piece)))
                }
                None if shift == 0.0 => piece,
                None => TilePiece::Clipped(owned(piece)),
            };
            by_key.insert(key, piece);
        }
    }
    by_key.into_iter().collect()
}

/// [`feature_tile_pieces`] with each whole piece a copy of `geom`.
pub(super) fn feature_tile_members(
    geom: &Geometry<f64>,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Vec<(u64, Geometry<f64>)> {
    feature_tile_pieces(geom, zoom, opts, key_lo, key_hi, diagnostics)
        .into_iter()
        .map(|(key, piece)| match piece {
            TilePiece::Whole => (key, geom.clone()),
            TilePiece::Clipped(g) => (key, g),
        })
        .collect()
}

/// A feature's decoded geometry, as [`shared_tile_members`] takes it: its
/// own, or shared with other levels (the partitioning fill, #815).
pub(super) enum MemberSource<'a> {
    Owned(Geometry<f64>),
    Shared(&'a Arc<Geometry<f64>>),
}

/// `source`'s members at `zoom`, at its finest-zoom simplification when
/// `finest` is set, as member geometries. A tile that holds the feature
/// whole takes the geometry instead of a copy (manby holds each feature
/// once): an owned geometry moves into its one whole tile, or into one `Arc`
/// that two or more share; a shared source is shared as it is. A point holds
/// no coordinate heap, so its tiles hold it inline.
pub(super) fn shared_tile_members(
    source: MemberSource<'_>,
    finest: Finest<'_>,
    zoom: u8,
    opts: &ExportOptions,
    (key_lo, key_hi): (u64, u64),
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Vec<(u64, MemberGeom)> {
    let src: &Geometry<f64> = match &source {
        MemberSource::Owned(g) => g,
        MemberSource::Shared(g) => g,
    };
    let point = matches!(src, Geometry::Point(_));
    let simplified = match finest_zoom_geometry(src, finest, opts, diagnostics) {
        std::borrow::Cow::Owned(g) => Some(g),
        std::borrow::Cow::Borrowed(_) => None,
    };
    let pieces = feature_tile_pieces(
        simplified.as_ref().unwrap_or(src),
        zoom,
        opts,
        key_lo,
        key_hi,
        diagnostics,
    );
    let mut left = pieces
        .iter()
        .filter(|(_, piece)| matches!(piece, TilePiece::Whole))
        .count();
    let (shared, mut owned) = match (simplified, source) {
        _ if left == 0 => (None, None),
        (None, MemberSource::Shared(g)) if !point => (Some(Arc::clone(g)), None),
        (None, MemberSource::Shared(g)) => (None, Some((**g).clone())),
        (Some(g), _) | (None, MemberSource::Owned(g)) if left > 1 && !point => {
            (Some(Arc::new(g)), None)
        }
        (Some(g), _) | (None, MemberSource::Owned(g)) => (None, Some(g)),
    };
    pieces
        .into_iter()
        .map(|(key, piece)| match piece {
            TilePiece::Whole => {
                left -= 1;
                let geom = match &shared {
                    Some(g) => MemberGeom::Shared(Arc::clone(g)),
                    // The last whole tile takes it; earlier ones (points) copy.
                    None if left == 0 => MemberGeom::Owned(owned.take().expect("whole geometry")),
                    None => MemberGeom::Owned(owned.clone().expect("whole geometry")),
                };
                (key, geom)
            }
            TilePiece::Clipped(g) => (key, MemberGeom::Owned(g)),
        })
        .collect()
}

/// Longitude shift of each world copy of a feature with `bbox` that reaches
/// the tile grid at a zoom whose edge buffer is `buffer` degrees (#342).
///
/// The stored copy (shift 0) comes first, unless it lies wholly outside the
/// world grown by the buffer: `expand_bbox` would collapse such a copy onto
/// the edge column, where the clip drops it, so planning it only inflates
/// that column's counts. A ±360° copy joins when it overlaps the buffered
/// world with positive width: a feature past +180° draws its
/// overhang at the west edge, a feature at 0..360 lands at its -180..0 twin,
/// and a feature within `buffer` of +180° fills the west-edge tile's buffer.
/// This is tippecanoe's world-edge rule: `clip_to_tile` at z0 appends a copy
/// shifted by one world width whenever the feature's bbox reaches within the
/// buffer of either edge (tile.cpp, "If the geometry extends off the edge of
/// the world"), and the children inherit both copies. Testing against the
/// zoom's own buffer rather than z0's skips only copies the clip would cut
/// away anyway, so the tiles are the same. A copy that only touches the
/// buffered world's edge holds no area inside it and is left out.
pub(super) fn wrap_shifts(bbox: &TileBounds, buffer: f64) -> impl Iterator<Item = f64> + '_ {
    [0.0, -360.0, 360.0]
        .into_iter()
        .filter(move |&shift: &f64| {
            let (lo, hi) = (bbox.lng_min + shift, bbox.lng_max + shift);
            if shift == 0.0 {
                // Edge contact counts for the stored copy, as it always has.
                hi >= -180.0 - buffer && lo <= 180.0 + buffer
            } else {
                hi > -180.0 - buffer && lo < 180.0 + buffer
            }
        })
}

/// `false` when [`wrap_shifts`] yields only the stored copy: the bbox stays
/// more than `buffer` inside both world edges. Every ±360° copy test implies
/// this one, so it is the cheap gate that keeps ordinary features off the
/// wrap path (no copy, no per-key join).
#[inline]
pub(super) fn near_world_edge(bbox: &TileBounds, buffer: f64) -> bool {
    bbox.lng_min < -180.0 + buffer || bbox.lng_max > 180.0 - buffer
}

/// `geom` moved `shift` degrees east. The stored copy borrows, so a large
/// polygon on the world edge is not cloned for it.
pub(super) fn shift_lng(geom: &Geometry<f64>, shift: f64) -> std::borrow::Cow<'_, Geometry<f64>> {
    if shift == 0.0 {
        return std::borrow::Cow::Borrowed(geom);
    }
    std::borrow::Cow::Owned(geom.map_coords(|c| geo::Coord {
        x: c.x + shift,
        y: c.y,
    }))
}

/// `bbox` moved `shift` degrees east.
pub(super) fn shift_bbox(bbox: &TileBounds, shift: f64) -> TileBounds {
    TileBounds::new(
        bbox.lng_min + shift,
        bbox.lat_min,
        bbox.lng_max + shift,
        bbox.lat_max,
    )
}

/// Join two world copies' pieces of one feature in one tile (#342). Pieces
/// of one family join into its multi type; mixed families, which no clip of
/// a single geometry produces, fall back to a collection, which the MVT
/// encoder splits by kind.
pub(super) fn join_wrapped_pieces(a: Geometry<f64>, b: Geometry<f64>) -> Geometry<f64> {
    use geo::{MultiLineString, MultiPoint, MultiPolygon};
    fn polygons(g: &Geometry<f64>) -> Option<Vec<geo::Polygon<f64>>> {
        match g {
            Geometry::Polygon(p) => Some(vec![p.clone()]),
            Geometry::MultiPolygon(m) => Some(m.0.clone()),
            _ => None,
        }
    }
    fn lines(g: &Geometry<f64>) -> Option<Vec<geo::LineString<f64>>> {
        match g {
            Geometry::LineString(l) => Some(vec![l.clone()]),
            Geometry::MultiLineString(m) => Some(m.0.clone()),
            _ => None,
        }
    }
    fn points(g: &Geometry<f64>) -> Option<Vec<geo::Point<f64>>> {
        match g {
            Geometry::Point(p) => Some(vec![*p]),
            Geometry::MultiPoint(m) => Some(m.0.clone()),
            _ => None,
        }
    }
    if let (Some(mut x), Some(y)) = (polygons(&a), polygons(&b)) {
        x.extend(y);
        return Geometry::MultiPolygon(MultiPolygon(x));
    }
    if let (Some(mut x), Some(y)) = (lines(&a), lines(&b)) {
        x.extend(y);
        return Geometry::MultiLineString(MultiLineString(x));
    }
    if let (Some(mut x), Some(y)) = (points(&a), points(&b)) {
        x.extend(y);
        return Geometry::MultiPoint(MultiPoint(x));
    }
    Geometry::GeometryCollection(geo::GeometryCollection(vec![a, b]))
}

/// One world copy's share of [`feature_tile_members`]: clip `geom` (whose
/// bbox is `bbox`) into every tile it belongs to at `zoom` within
/// `[key_lo, key_hi]`, appending to `out`.
pub(super) fn copy_tile_members(
    geom: &Geometry<f64>,
    bbox: &TileBounds,
    inputs: TileTarget<'_>,
    out: &mut Vec<(u64, TilePiece)>,
) {
    let TileTarget {
        zoom,
        opts,
        key_lo,
        key_hi,
        assume_simple,
        diagnostics,
        source,
    } = inputs;
    let bbox = *bbox;
    // Target leaf-tile ranges at `zoom` — the authority for which tiles this
    // feature belongs to. Derived from the BUFFER-EXPANDED bbox so a feature
    // sitting in a neighbouring tile's buffer is a member of that tile; the
    // bare `bbox` is still what the containment fast paths test against, and
    // candidates that turn out not to intersect the buffered tile are dropped
    // by the clip below.
    let ranges = member_ranges(&bbox, zoom, opts);

    // Dispatch on the *direct* path's cost — the ticket's own model,
    // `tiles_spanned × vertex_count`. The recursive cascade wins big when that
    // product explodes (few huge admin polygons: high span AND high vertex
    // count, the adm4 blowup), but its per-node overhead (an intermediate clip
    // + trig-heavy `TileCoord::bounds()` at every pyramid node) is dead weight
    // for the common case of many features that are already small — overview
    // levels are pre-simplified, so a deep-zoom feature carries few vertices and
    // the plain per-tile clip is cheaper. Routing those through the direct loop
    // keeps them byte-identical to the pre-#226 path and avoids a regression on
    // dense corpora, while the giant polygons that actually caused the DNF take
    // the cascade.
    // `assume_simple` is the feature's ring simplicity, validated **once** by
    // the caller and threaded through every clip this feature performs (issue
    // #237, RC3). The O((n + m) log n) self-intersection sweep that
    // `clip_polygon` used to run on *every* tile now runs a single time per
    // feature; a continental
    // polygon touching thousands of tiles pays it once instead of thousands of
    // times. The clip output is unchanged — see `clip::clip_geometry_simple`.
    let direct_cost = tile_span(&ranges).saturating_mul(geom.coords_count() as u64);
    if direct_cost <= DIRECT_CLIP_BUDGET {
        feature_tile_members_direct(
            geom,
            &bbox,
            TileTarget {
                zoom,
                opts,
                key_lo,
                key_hi,
                assume_simple,
                diagnostics,
                source,
            },
            &ranges,
            out,
        );
    } else {
        // Start the descent at the feature's covering tile (the deepest tile
        // whose bounds contain the whole feature bbox) rather than the world
        // root: every level above it is a single-child pass-through, so
        // skipping them is free and yields an identical leaf set.
        let root = covering_tile(&ranges, zoom);
        split_feature_into_tiles(
            root,
            geom,
            &bbox,
            TileTarget {
                zoom,
                opts,
                key_lo,
                key_hi,
                assume_simple,
                diagnostics,
                source,
            },
            &ranges,
            out,
        );
    }
}

/// Longitude degrees corresponding to the edge buffer at `zoom`, under the same
/// convention the clip uses (`tile_width * buffer_px / 256`, applied to both
/// axes — see [`bbox_within_buffered`]).
///
/// DIVERGENCE FROM TIPPECANOE: this single degree value is applied to BOTH
/// axes, so it is exactly `buffer_px` only on the x axis. Because a Mercator
/// tile's latitude span shrinks as `cos(lat)` while its longitude span does
/// not, the vertical buffer is effectively `buffer_px * sec(lat)` pixels —
/// 8 px at the equator, ~16 px at 60°, ~92 px at 85°. Tippecanoe buffers in
/// tile pixels on both axes. The over-draw is bounded (it stays under one tile
/// height below ~88.2° latitude, outside the Mercator domain) and it errs
/// towards carrying more geometry across the seam rather than less, so it is
/// documented rather than fixed here: a per-axis buffer has to be threaded
/// through `bbox_within_buffered` and `clip_geometry_simple` too, which moves
/// tile bytes again. See `context/ARCHITECTURE.md`.
///
/// Tile MEMBERSHIP must be computed from the buffer-expanded bbox, not the bare
/// one: a feature lying outside a tile but inside its buffer still has to reach
/// that tile, or the neighbour has nothing to draw the overlapping part from and
/// the feature is sliced at the seam. Points showed this most starkly — with the
/// bare bbox every point lands in exactly one tile and a symbol straddling a
/// seam renders as a half circle.
#[inline]
pub(super) fn buffer_deg_at_zoom(zoom: u8, opts: &ExportOptions) -> f64 {
    // #371: `1u32 << zoom` overflowed at z32 — a debug panic, and in release a
    // masked shift that produced a silently wrong buffer for every tile.
    // `powi` is exact for these powers of two and total for any u8 zoom.
    let tile_width = 360.0 / 2f64.powi(i32::from(zoom));
    tile_width * buffer_fraction(opts)
}

/// `bbox` grown by `buffer` on every side, clamped to the valid lon/lat domain
/// so the expansion cannot manufacture a wrapped (`lng_min > lng_max`) bbox and
/// flip [`tile_ranges_for_bbox`] onto its antimeridian branch.
///
/// Clamping each edge independently is NOT enough. `lng_min` floored at -180
/// and `lng_max` capped at +180 cross over for a bbox lying wholly outside the
/// domain — a point at lng 180.5, say, which a dataset using the 0..360
/// convention is full of. The result reads as wrapped, and
/// `tile_ranges_for_bbox` then hands back every tile column at the zoom:
/// 16,385 of them at z14, per feature, for a feature that ends up in none of
/// them. So the edges are re-ordered back after clamping.
///
/// The clamp keeps each copy's membership on its own side of the
/// antimeridian. Continuity across it comes from [`wrap_shifts`] instead: a
/// feature at lng 179.99 reaches tile x=0 through its copy at -180.01 (#342).
#[inline]
pub(super) fn expand_bbox(bbox: &TileBounds, buffer: f64) -> TileBounds {
    // A bbox that was ALREADY wrapped on the way in must stay wrapped; only a
    // wrap newly manufactured by the clamp is collapsed.
    let was_wrapped = bbox.lng_min > bbox.lng_max;
    let mut lng_min = (bbox.lng_min - buffer).clamp(-180.0, 180.0);
    let lng_max = (bbox.lng_max + buffer).clamp(-180.0, 180.0);
    if !was_wrapped && lng_min > lng_max {
        // Wholly outside the domain: collapse onto the edge it was clamped to
        // rather than claiming the world.
        lng_min = lng_max;
    }
    TileBounds::new(
        lng_min,
        (bbox.lat_min - buffer).clamp(-90.0, 90.0),
        lng_max,
        (bbox.lat_max + buffer).clamp(-90.0, 90.0),
    )
}

/// The leaf-tile ranges a feature with `bbox` belongs to at `zoom` — the buffer
/// widening applied in ONE place.
///
/// The scan (`size_bboxes`, which plans the partitions) and the emit
/// (`feature_tile_members`, which fills them) must agree on this set exactly:
/// the export requires every emitted key to land in a planned partition, and
/// `route_partition` only `debug_assert!`s that, so a release build would
/// silently write tiles out of key order instead of failing. Sharing one helper
/// makes them agree by construction rather than by matching edits.
#[inline]
pub(super) fn member_ranges(bbox: &TileBounds, zoom: u8, opts: &ExportOptions) -> BboxTileRanges {
    tile_ranges_for_bbox(&expand_bbox(bbox, buffer_deg_at_zoom(zoom, opts)), zoom)
}

/// Direct-path cost budget, in `tiles_spanned × vertices` clip-vertex ops. At
/// or below it the per-tile direct clip beats the recursive cascade's per-node
/// overhead; above it the cascade's `O(depth)` vertex participation wins. Tuned
/// so pre-simplified overview features (low vertex count, modest span) stay on
/// the direct — and byte-identical — path, and only genuinely huge polygons
/// recurse.
pub(super) const DIRECT_CLIP_BUDGET: u64 = 100_000;

/// Cascade depth (levels remaining below a node) above which the four child
/// subtrees are split across Rayon threads (issue #237, RC4).
///
/// The wave already runs one `par_iter` task per feature, but a single
/// continental polygon — the adm4 straggler carries ~100k vertices and covers
/// thousands of tiles — serializes an entire level on one core while the other
/// 15 sit idle once the small features finish. Forking its cascade lets those
/// idle cores steal the subtrees. Only the coarse top of a large cascade forks
/// (a node this shallow roots ≥ `4^DEPTH` leaves, so the fork amortizes); the
/// dense lower levels recurse sequentially to avoid drowning the pool in
/// microtasks. Small features take the direct path and never reach here.
pub(super) const CASCADE_PARALLEL_DEPTH: u32 = 3;

/// Vertices in a cascade node's piece at which its children fork whatever
/// the depth (M1, #817): every child clips the whole piece, so a monster
/// coastline at a coarse zoom otherwise clips on one thread. The same
/// break-even reasoning as [`PARALLEL_ENCODE_MIN_COORDS`].
pub(super) const CASCADE_PARALLEL_COORDS: usize = 4096;

#[cfg(test)]
thread_local! {
    /// Cascade nodes that split their children across Rayon tasks.
    pub(super) static CASCADE_FORKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Number of leaf tiles the feature's bbox covers at the target zoom (both
/// x-bands when it crosses the antimeridian).
#[inline]
pub(super) fn tile_span(ranges: &BboxTileRanges) -> u64 {
    let x1 = (ranges.x.1 - ranges.x.0 + 1) as u64;
    let x2 = ranges.x2.map_or(0, |(a, b)| (b - a + 1) as u64);
    let y = (ranges.y.1 - ranges.y.0 + 1) as u64;
    (x1 + x2) * y
}

/// The pre-#226 direct path: enumerate every tile the feature bbox covers and
/// clip the feature's full geometry against each (fast-pathing interior
/// features). Used for features whose direct cost is under
/// [`DIRECT_CLIP_BUDGET`]; its output is byte-identical to the historical
/// export. Appends `(key, geom)` members within `[key_lo, key_hi]` to `out`.
/// `assume_simple` is the per-feature ring-simplicity hint (issue #237, RC3).
pub(super) fn feature_tile_members_direct(
    geom: &Geometry<f64>,
    bbox: &TileBounds,
    inputs: TileTarget<'_>,
    ranges: &BboxTileRanges,
    out: &mut Vec<(u64, TilePiece)>,
) {
    let TileTarget {
        zoom,
        opts,
        key_lo,
        key_hi,
        assume_simple,
        diagnostics,
        source: _,
    } = inputs;
    for tc in tiles_in_ranges(ranges, zoom) {
        let key = tile_key(tc.x, tc.y, zoom);
        if key < key_lo || key > key_hi {
            continue;
        }
        let tb = tc.bounds();
        let buffer_deg = tb.width() * buffer_fraction(opts);
        if bbox_within_buffered(bbox, &tb, buffer_deg) {
            out.push((key, TilePiece::Whole));
        } else if let Some(clipped) = clip_geometry_simple(
            geom,
            &tb,
            buffer_deg,
            assume_simple,
            opts.simple_clip_fastpath,
            diagnostics,
        ) {
            out.push((key, TilePiece::Clipped(clipped)));
        }
    }
}

/// Every leaf tile in `ranges` at `zoom`, both x-bands when the bbox wraps.
/// The enumeration authority for the direct path, so it and the cascade (which
/// prunes on the same `ranges`) agree on the leaf set by construction.
pub(super) fn tiles_in_ranges(
    ranges: &BboxTileRanges,
    zoom: u8,
) -> impl Iterator<Item = TileCoord> + '_ {
    let bands = std::iter::once(ranges.x).chain(ranges.x2);
    bands.flat_map(move |(x0, x1)| {
        (x0..=x1)
            .flat_map(move |x| (ranges.y.0..=ranges.y.1).map(move |y| TileCoord::new(x, y, zoom)))
    })
}

/// The deepest tile whose subtree contains every tile the feature covers — the
/// common ancestor of the target range corners. Starting the cascade here
/// instead of at the world root skips the pure single-child descent while
/// producing an identical leaf set. Antimeridian-crossing ranges (two x-bands)
/// have no single covering tile below the root, so fall back to `(0, 0, 0)`.
#[inline]
pub(super) fn covering_tile(ranges: &BboxTileRanges, zoom: u8) -> TileCoord {
    if ranges.x2.is_some() {
        return TileCoord::new(0, 0, 0);
    }
    let (x_lo, x_hi) = ranges.x;
    let (y_lo, y_hi) = ranges.y;
    // Smallest shift `s` at which both corners share a tile = deepest common
    // ancestor (at zoom `zoom - s`).
    let mut s = 0u8;
    while s < zoom && !((x_lo >> s) == (x_hi >> s) && (y_lo >> s) == (y_hi >> s)) {
        s += 1;
    }
    TileCoord::new(x_lo >> s, y_lo >> s, zoom - s)
}

/// `true` when tile node `(x, y, z)`'s descendant-leaf footprint at `zoom`
/// overlaps the target ranges (i.e. the node's subtree contains at least one
/// tile the feature bbox covers).
#[inline]
pub(super) fn node_overlaps_ranges(node: TileCoord, zoom: u8, ranges: &BboxTileRanges) -> bool {
    let shift = (zoom - node.z) as u32;
    let x_lo = (node.x as u64) << shift;
    let x_hi = (((node.x as u64) + 1) << shift) - 1;
    let y_lo = (node.y as u64) << shift;
    let y_hi = (((node.y as u64) + 1) << shift) - 1;
    let overlaps = |a: (u32, u32), b: (u64, u64)| a.0 as u64 <= b.1 && b.0 <= a.1 as u64;
    if !overlaps(ranges.y, (y_lo, y_hi)) {
        return false;
    }
    overlaps(ranges.x, (x_lo, x_hi)) || ranges.x2.is_some_and(|x2| overlaps(x2, (x_lo, x_hi)))
}

/// `true` when tile node `(x, y, z)`'s descendant-leaf key range overlaps the
/// partition key window `[key_lo, key_hi]`. Keys are PMTiles tile ids
/// (#506), so a node's subtree spans an EXACT contiguous interval at `zoom`
/// — [`crate::tile::node_id_range`] — rather than the conservative,
/// gap-riddled bounding box the row-major key used to produce. The test is
/// therefore now precise, not just safe: it never skips a needed tile (the
/// exact leaf key guard still drops any non-member on top of this), and it no
/// longer visits branches whose subtree merely bounds, without overlapping,
/// the window. It is what keeps a giant feature's per-partition work bounded
/// to the partition's slice instead of re-walking the whole subtree once per
/// partition it spans.
#[inline]
pub(super) fn node_key_overlaps(node: TileCoord, zoom: u8, key_lo: u64, key_hi: u64) -> bool {
    let range = crate::tile::node_id_range(node, zoom);
    *range.start() <= key_hi && key_lo <= *range.end()
}

pub(super) fn split_feature_into_tiles(
    node: TileCoord,
    cur: &Geometry<f64>,
    cur_bbox: &TileBounds,
    inputs: TileTarget<'_>,
    ranges: &BboxTileRanges,
    out: &mut Vec<(u64, TilePiece)>,
) {
    let TileTarget {
        zoom,
        opts,
        key_lo,
        key_hi,
        assume_simple,
        diagnostics,
        source,
    } = inputs;
    // Prune: outside the feature's target tiles, or outside this partition.
    if !node_overlaps_ranges(node, zoom, ranges) || !node_key_overlaps(node, zoom, key_lo, key_hi) {
        return;
    }

    let tb = node.bounds();
    let buffer_deg = tb.width() * buffer_fraction(opts);

    if node.z == zoom {
        // Leaf tile. The prune above already proved this tile is in the
        // feature's target set; enforce the exact partition key window here.
        let key = tile_key(node.x, node.y, zoom);
        if key < key_lo || key > key_hi {
            return;
        }
        // Fast path (H3(c) lever 4): a feature whose bbox lies entirely within
        // the buffered tile is unaffected by clipping — emit as-is. For a
        // feature interior to the leaf this reproduces the direct loop's
        // `geom.clone()` byte-for-byte, since every ancestor also fast-pathed
        // and `cur` is still the original geometry.
        if bbox_within_buffered(cur_bbox, &tb, buffer_deg) {
            let piece = if std::ptr::eq(cur, source) {
                TilePiece::Whole
            } else {
                TilePiece::Clipped(cur.clone())
            };
            out.push((key, piece));
        } else if let Some(clipped) = clip_geometry_simple(
            cur,
            &tb,
            buffer_deg,
            assume_simple,
            opts.simple_clip_fastpath,
            diagnostics,
        ) {
            out.push((key, TilePiece::Clipped(clipped)));
        }
        return;
    }

    // Internal node: shrink `cur` to this node's buffered bounds before
    // splitting, so each child clips an already-halved geometry. Clipping to
    // `node ± buffer` (not the bare node) preserves the superset chain that
    // makes the cascade equal to a direct leaf clip.
    let children = node.children().expect("non-leaf node has children");
    if bbox_within_buffered(cur_bbox, &tb, buffer_deg) {
        // Feature already inside this node's buffered bounds — no clip needed;
        // descend with the same geometry (this is what makes the coarse top of
        // the pyramid essentially free).
        fanout_children(
            children,
            node.z,
            cur,
            cur_bbox,
            TileTarget {
                zoom,
                opts,
                key_lo,
                key_hi,
                assume_simple,
                diagnostics,
                source,
            },
            ranges,
            out,
        );
    } else if let Some(clipped) = clip_geometry_simple(
        cur,
        &tb,
        buffer_deg,
        assume_simple,
        opts.simple_clip_fastpath,
        diagnostics,
    ) {
        let cbbox = match clipped.bounding_rect() {
            Some(r) => TileBounds::new(r.min().x, r.min().y, r.max().x, r.max().y),
            None => return,
        };
        fanout_children(
            children,
            node.z,
            &clipped,
            &cbbox,
            TileTarget {
                zoom,
                opts,
                key_lo,
                key_hi,
                assume_simple,
                diagnostics,
                source,
            },
            ranges,
            out,
        );
    }
    // else: `cur` does not intersect this node's buffered bounds — whole
    // subtree pruned.
}

/// Recurse into a node's four children with `child_geom` (already reduced to the
/// parent's buffered bounds). Near the top of a large cascade (issue #237, RC4),
/// or under a piece of [`CASCADE_PARALLEL_COORDS`] vertices, the four subtrees
/// run as separate Rayon tasks; otherwise it stays sequential. Emission order is irrelevant:
/// `finish_group` buckets members by tile key, and a feature emits at
/// most one member per key, so parallel interleaving cannot change the archive.
pub(super) fn fanout_children(
    children: [TileCoord; 4],
    node_z: u8,
    child_geom: &Geometry<f64>,
    child_bbox: &TileBounds,
    inputs: TileTarget<'_>,
    ranges: &BboxTileRanges,
    out: &mut Vec<(u64, TilePiece)>,
) {
    let TileTarget {
        zoom,
        opts,
        key_lo,
        key_hi,
        assume_simple,
        diagnostics,
        source,
    } = inputs;
    // M1 (#817): each child clips the whole parent piece, so a huge piece
    // forks at any depth; the four subtrees' members are appended in child
    // order, as the sequential loop emits them.
    if (zoom - node_z) as u32 > CASCADE_PARALLEL_DEPTH
        || child_geom.coords_count() >= CASCADE_PARALLEL_COORDS
    {
        #[cfg(test)]
        CASCADE_FORKS.with(|c| c.set(c.get() + 1));
        let subtrees: Vec<Vec<(u64, TilePiece)>> = children
            .par_iter()
            .map(|&child| {
                let mut members = Vec::new();
                split_feature_into_tiles(
                    child,
                    child_geom,
                    child_bbox,
                    TileTarget {
                        zoom,
                        opts,
                        key_lo,
                        key_hi,
                        assume_simple,
                        diagnostics,
                        source,
                    },
                    ranges,
                    &mut members,
                );
                members
            })
            .collect();
        for mut members in subtrees {
            out.append(&mut members);
        }
    } else {
        for child in children {
            split_feature_into_tiles(
                child,
                child_geom,
                child_bbox,
                TileTarget {
                    zoom,
                    opts,
                    key_lo,
                    key_hi,
                    assume_simple,
                    diagnostics,
                    source,
                },
                ranges,
                out,
            );
        }
    }
}

/// Test helper: run the direct path unconditionally (bypassing the dispatch
/// budget) and collect its members — the oracle the recursive cascade is
/// verified against.
#[cfg(test)]
pub(super) fn members_direct_vec(
    geom: &Geometry<f64>,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
) -> Vec<(u64, Geometry<f64>)> {
    let Some(rect) = geom.bounding_rect() else {
        return Vec::new();
    };
    let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
    let ranges = member_ranges(&bbox, zoom, opts);
    let assume_simple = geometry_is_simple(geom);
    let mut out = Vec::new();
    feature_tile_members_direct(
        geom,
        &bbox,
        TileTarget {
            zoom,
            opts,
            key_lo,
            key_hi,
            assume_simple,
            diagnostics: &crate::diagnostics::Diagnostics::default(),
            source: geom,
        },
        &ranges,
        &mut out,
    );
    out.into_iter()
        .map(|(key, piece)| match piece {
            TilePiece::Whole => (key, geom.clone()),
            TilePiece::Clipped(g) => (key, g),
        })
        .collect()
}

/// Test helper: run the recursive cascade unconditionally (bypassing the
/// dispatch budget) and collect its members.
#[cfg(test)]
pub(super) fn members_recursive_vec(
    geom: &Geometry<f64>,
    zoom: u8,
    opts: &ExportOptions,
    key_lo: u64,
    key_hi: u64,
) -> Vec<(u64, Geometry<f64>)> {
    let Some(rect) = geom.bounding_rect() else {
        return Vec::new();
    };
    let bbox = TileBounds::new(rect.min().x, rect.min().y, rect.max().x, rect.max().y);
    // Same widening as `feature_tile_members`, or the oracle compares the
    // cascade over a narrower tile set than the direct path walks.
    let ranges = member_ranges(&bbox, zoom, opts);
    let root = covering_tile(&ranges, zoom);
    let assume_simple = geometry_is_simple(geom);
    let mut out = Vec::new();
    split_feature_into_tiles(
        root,
        geom,
        &bbox,
        TileTarget {
            zoom,
            opts,
            key_lo,
            key_hi,
            assume_simple,
            diagnostics: &crate::diagnostics::Diagnostics::default(),
            source: geom,
        },
        &ranges,
        &mut out,
    );
    out.into_iter()
        .map(|(key, piece)| match piece {
            TilePiece::Whole => (key, geom.clone()),
            TilePiece::Clipped(g) => (key, g),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use geo::LineString;

    use super::super::tests::*;
    use super::*;

    // --- recursive splitter equivalence (issue #226) -------------------------

    /// #750: the fill may take a quarter of a tile's size limit (a compressed
    /// limit read as twice that many raw bytes), so the size valve does not
    /// have to squeeze the tile's own features into half of it.
    #[test]
    fn the_fill_tile_bytes_are_a_quarter_of_the_limit() {
        let raw = ExportOptions {
            tile_size_limit: Some(1000),
            compressed_tile_size_limit: None,
            ..ExportOptions::default()
        };
        assert_eq!(fill_tile_bytes(&raw), 250);
        let gz = ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: Some(1000),
            ..ExportOptions::default()
        };
        assert_eq!(fill_tile_bytes(&gz), 500);
        let none = ExportOptions {
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..ExportOptions::default()
        };
        assert_eq!(fill_tile_bytes(&none), usize::MAX);
    }

    /// #750: fill features are drawn but not counted as tile features, so the
    /// report and tilestats count the data's own features, and a tile whose
    /// features all failed to encode is not written for its fill alone.
    #[test]
    fn fill_features_do_not_count_as_tile_features() {
        let tb = TileCoord::new(512, 300, 10).bounds();
        let fill = [fill_square(7, 0, 0, 100)];
        let mvt = build_mvt::<Member>(
            &test_gzip_cache(),
            &[],
            &tb,
            &ExportOptions::default(),
            &[&fill[0]],
            &crate::diagnostics::Diagnostics::default(),
        );
        assert_eq!(mvt.features, 0);
        assert!(!mvt.data.is_empty(), "the fill is still encoded");
        assert_eq!(
            mvt.source_ids,
            vec![7],
            "drawn fill retains its host source identity"
        );
    }

    /// A z10 tile of 400 small, merge-able squares with a tag each, and a
    /// fill of 80 squares between them.
    fn crowded_tile() -> (TileBounds, Vec<Member>, Vec<FillFeature>) {
        let tb = TileCoord::new(512, 300, 10).bounds();
        let (dx, dy) = (
            (tb.lng_max - tb.lng_min) / 40.0,
            (tb.lat_max - tb.lat_min) / 40.0,
        );
        let members: Vec<Member> = (0..400u64)
            .map(|i| {
                let (c, r) = ((i % 20) as f64, (i / 20) as f64);
                let (x, y) = (tb.lng_min + 2.0 * c * dx, tb.lat_min + 2.0 * r * dy);
                Member {
                    key: 0,
                    seq: i,
                    rank: point_rank(i),
                    geom: Geometry::Polygon(
                        geo::Rect::new(
                            geo::coord! { x: x, y: y },
                            geo::coord! { x: x + dx, y: y + dy },
                        )
                        .to_polygon(),
                    )
                    .into(),
                    props: SharedProps::new(vec![(Arc::from("n"), PropertyValue::UInt(i))]),
                    id: Some(i),
                    gap: if i == 0 { GAP_NONE } else { 10 },
                    band: 0,
                }
            })
            .collect();
        let fill: Vec<FillFeature> = (0..80)
            .map(|i| fill_square(i as u64, 50 * (i % 40) + 2000, 100 * (i / 40) + 2000, 40))
            .collect();
        (tb, members, fill)
    }

    /// #788 phase 1: this guard is already green on main. Fill geometry is
    /// encoded once; the repeated work is the tile builder and fit checks.
    #[test]
    fn phase1_first_fit_encodes_fill_polygons_once() {
        coverage_fill::cost_probe::take();
        let (tb, members, fill) = crowded_tile();
        let polygons: usize = fill.iter().map(|f| f.polygons).sum();
        let mut tally = FillTally::default();
        let mvt = encode_tile(
            &test_gzip_cache(),
            &members,
            &tb,
            &ExportOptions::default(),
            &fill,
            &mut tally,
            &crate::diagnostics::Diagnostics::default(),
        );
        let counts = coverage_fill::cost_probe::take();
        assert_eq!(tally.polygons, polygons);
        assert_eq!(mvt.valve.passes, 2, "one base encode plus one fill pass");
        assert_eq!(counts.fill_polygons_encoded, polygons, "{counts:?}");
        assert_eq!(counts.fill_passes, 1, "{counts:?}");
    }

    /// #767: the base tile already has gzip bytes after its valve fit check.
    /// `with_fill` must reuse that answer before attempting its first fill.
    #[test]
    fn phase1_fill_does_not_recompress_the_cached_base_tile() {
        coverage_fill::cost_probe::take();
        let (tb, members, fill) = crowded_tile();
        let mut tally = FillTally::default();
        let mvt = encode_tile(
            &test_gzip_cache(),
            &members,
            &tb,
            &ExportOptions {
                compressed_tile_size_limit: Some(500_000),
                ..Default::default()
            },
            &fill,
            &mut tally,
            &crate::diagnostics::Diagnostics::default(),
        );
        let counts = coverage_fill::cost_probe::take();
        assert!(mvt.gzip.is_some());
        assert_eq!(counts.fill_passes, 1);
        assert_eq!(
            counts.cached_gzip_rechecks, 0,
            "a cached base tile was sent through gzip again: {counts:?}"
        );
    }

    /// #750: the fill takes only the room the tile's own features leave. A
    /// tile whose features fit, but not with all of its fill, sheds fill;
    /// no feature is merged or dropped for it.
    #[test]
    fn the_fill_only_takes_the_slack_its_features_leave() {
        let (tb, members, fill) = crowded_tile();
        let bare = encode_tile(
            &test_gzip_cache(),
            &members,
            &tb,
            &ExportOptions::default(),
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        );
        let all: Vec<&FillFeature> = fill.iter().collect();
        let full = build_mvt(
            &test_gzip_cache(),
            &members,
            &tb,
            &ExportOptions::default(),
            &all,
            &crate::diagnostics::Diagnostics::default(),
        );
        let limit = bare.data.len() + (full.data.len() - bare.data.len()) / 2;
        for valve in [SizeValve::Merge, SizeValve::Drop] {
            let opts = ExportOptions {
                tile_size_limit: Some(limit),
                compressed_tile_size_limit: None,
                size_valve: valve,
                ..ExportOptions::default()
            };
            let mut tally = FillTally::default();
            let mvt = encode_tile(
                &test_gzip_cache(),
                &members,
                &tb,
                &opts,
                &fill,
                &mut tally,
                &crate::diagnostics::Diagnostics::default(),
            );
            assert_eq!((mvt.valve.merged, mvt.valve.dropped), (0, 0), "{valve:?}");
            assert_eq!(mvt.features, members.len(), "{valve:?}");
            assert!(
                mvt.data.len() <= limit,
                "{valve:?}: {} > {limit}",
                mvt.data.len()
            );
            assert!(tally.shed > 0 && tally.polygons > 0, "{valve:?}: {tally:?}");
        }
    }

    /// #750: a tile with no room for any of its fill is written without it,
    /// and the encodes spent trying still count as valve passes, so the
    /// work ratchets see them.
    #[test]
    fn fill_that_never_fits_still_counts_its_encodes() {
        let (tb, members, fill) = crowded_tile();
        let bare = encode_tile(
            &test_gzip_cache(),
            &members,
            &tb,
            &ExportOptions::default(),
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        );
        let opts = ExportOptions {
            tile_size_limit: Some(bare.data.len()),
            compressed_tile_size_limit: None,
            ..ExportOptions::default()
        };
        let mut tally = FillTally::default();
        let mvt = encode_tile(
            &test_gzip_cache(),
            &members,
            &tb,
            &opts,
            &fill,
            &mut tally,
            &crate::diagnostics::Diagnostics::default(),
        );
        assert_eq!(mvt.data, bare.data, "the tile goes without its fill");
        assert_eq!(tally.polygons, 0);
        assert!(mvt.valve.passes > 1, "{:?}", mvt.valve);
    }

    /// #750: on a tile over its limit without the fill, the valve decides
    /// exactly as with no fill at all.
    #[test]
    fn the_fill_does_not_change_what_the_valve_keeps() {
        let (tb, members, fill) = crowded_tile();
        let bare_size = encode_tile(
            &test_gzip_cache(),
            &members,
            &tb,
            &ExportOptions::default(),
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        )
        .data
        .len();
        for valve in [SizeValve::Merge, SizeValve::Drop] {
            let opts = ExportOptions {
                tile_size_limit: Some(bare_size / 2),
                compressed_tile_size_limit: None,
                size_valve: valve,
                ..ExportOptions::default()
            };
            let without = encode_tile(
                &test_gzip_cache(),
                &members,
                &tb,
                &opts,
                &[],
                &mut FillTally::default(),
                &crate::diagnostics::Diagnostics::default(),
            );
            let with = encode_tile(
                &test_gzip_cache(),
                &members,
                &tb,
                &opts,
                &fill,
                &mut FillTally::default(),
                &crate::diagnostics::Diagnostics::default(),
            );
            assert!(
                without.valve.merged + without.valve.dropped > 0,
                "{valve:?}"
            );
            assert_eq!(
                (with.valve.merged, with.valve.dropped, with.features),
                (
                    without.valve.merged,
                    without.valve.dropped,
                    without.features
                ),
                "{valve:?}"
            );
            assert!(
                with.data.len() <= bare_size / 2 || with.data == without.data,
                "{valve:?}"
            );
        }
    }

    /// #750: each fill polygon is drawn with the id and tags of the feature
    /// it lies in, one fill feature per host, so an attribute-styled map
    /// colours the fill like its neighbours.
    #[test]
    fn fill_polygons_carry_their_host_features_tags() {
        let zoom = 10;
        let tb = TileCoord::new(512, 300, zoom).bounds();
        let mid = (tb.lng_min + tb.lng_max) / 2.0;
        let half = |lng0: f64, lng1: f64, name: &str, id: u64| Member {
            key: 0,
            seq: 0,
            rank: point_rank(0),
            geom: Geometry::Polygon(
                geo::Rect::new(
                    geo::coord! { x: lng0, y: tb.lat_min },
                    geo::coord! { x: lng1, y: tb.lat_max },
                )
                .to_polygon(),
            )
            .into(),
            props: SharedProps::new(vec![(
                Arc::from("name"),
                PropertyValue::String(name.to_string()),
            )]),
            id: Some(id),
            gap: GAP_NONE,
            band: 0,
        };
        let members = [
            half(tb.lng_min, mid, "west", 7),
            half(mid, tb.lng_max, "east", 9),
        ];
        let square =
            |x: i32, y: i32| vec![vec![(x, y), (x + 100, y), (x + 100, y + 100), (x, y + 100)]];
        // Two polygons in the west half, one in the east half.
        let polys = vec![square(100, 100), square(3000, 2000), square(300, 3000)];
        let (fill, tally) = fill_features(
            &polys,
            &members,
            &tb,
            &ExportOptions::default(),
            &crate::diagnostics::Diagnostics::default(),
        );
        assert_eq!(
            tally,
            FillTally {
                polygons: 3,
                vertices: 12,
                shed: 0
            }
        );
        let tagged: Vec<(Option<u64>, String)> = fill
            .iter()
            .map(|f| match f.props.iter().next().unwrap().1 {
                PropertyRef::String(s) => (f.id, s.to_owned()),
                other => panic!("unexpected tag {other:?}"),
            })
            .collect();
        assert_eq!(
            tagged,
            vec![(Some(7), "west".to_string()), (Some(9), "east".to_string())]
        );
        assert!(fill.iter().all(|f| !f.cmds.is_empty()));
    }

    /// The recursive cascade must emit the same tile-key set as the direct
    /// (full-geometry, per-tile) oracle, and each tile's encoded MVT bytes must
    /// match.
    fn assert_recursive_matches_direct(geom: &Geometry<f64>, zoom: u8, opts: &ExportOptions) {
        let rec = members_by_key(members_recursive_vec(geom, zoom, opts, 0, u64::MAX));
        let dir = members_by_key(members_direct_vec(geom, zoom, opts, 0, u64::MAX));
        let mut rk: Vec<u64> = rec.keys().copied().collect();
        rk.sort_unstable();
        let mut dk: Vec<u64> = dir.keys().copied().collect();
        dk.sort_unstable();
        assert_eq!(rk, dk, "tile key set diverges at z{zoom}");
        for k in rk {
            let (kx, ky) = key_to_xy(k, zoom);
            assert_eq!(
                tile_mvt(k, &rec[&k], zoom, opts),
                tile_mvt(k, &dir[&k], zoom, opts),
                "tile ({kx}, {ky}) MVT diverges at z{zoom}",
            );
        }
    }

    /// M1 (#817): a monster coastline holds tens of thousands of vertices in
    /// one feature. At a coarse zoom its whole cascade ran on one thread:
    /// each child re-clips the parent's piece, four times per node.
    #[test]
    fn a_huge_feature_forks_its_cascade_at_coarse_zooms() {
        let coast = Geometry::LineString(LineString::from(
            (0..20_000)
                .map(|k| {
                    let t = f64::from(k) / 20_000.0;
                    (-150.0 + 300.0 * t, 60.0 * (t * 40.0).sin())
                })
                .collect::<Vec<_>>(),
        ));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .build()
            .unwrap();
        let forks = pool.install(|| {
            CASCADE_FORKS.with(|c| c.set(0));
            members_recursive_vec(&coast, 3, &ExportOptions::default(), 0, u64::MAX);
            CASCADE_FORKS.with(|c| c.get())
        });
        assert!(forks > 0, "the z3 cascade never forked");
        assert_recursive_matches_direct(&coast, 3, &ExportOptions::default());
    }

    /// Cross-check the recursive cascade against the direct oracle across a
    /// corpus that exercises interior, seam-crossing, concave, wide, dense and
    /// antimeridian features at a range of zooms (incl. depth-14 cascades). The
    /// full unbounded key range stands in for a single all-covering partition.
    #[test]
    fn recursive_split_matches_direct_oracle() {
        // Pin the fast path OFF: this oracle asserts the recursive cascade and
        // the direct clip produce byte-identical MVT, which is a property of the
        // splitter (#226) and holds only under the deterministic i_overlay clip.
        // With `simple_clip_fastpath` on (the default), Sutherland–Hodgman
        // rotates a clipped simple ring to a different start vertex, and the
        // rotation differs between the recursive-halving and direct-clip paths —
        // render-equivalent but not byte-identical, so the two paths legitimately
        // diverge in bytes here. That render-equivalence is covered separately by
        // the `clip.rs` `fastpath_*_render_equivalent` tests.
        let opts = ExportOptions {
            simple_clip_fastpath: false,
            ..Default::default()
        };
        for (name, geom, zooms) in equivalence_corpus() {
            for z in zooms {
                // Panics carry the feature name via the zoom-tagged messages;
                // prefix here for quick triage.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    assert_recursive_matches_direct(&geom, z, &opts);
                }))
                .unwrap_or_else(|_| panic!("equivalence failed for feature '{name}' at z{z}"));
            }
        }
    }

    /// `tile_buffer` is documented — and named — in TILE PIXELS, the unit
    /// tippecanoe's `--buffer` uses. It was divided by `extent`, the MVT
    /// coordinate resolution, making the real buffer 16x narrower than the
    /// number said (the default 8 "pixels" was half a pixel) and coupling it to
    /// a knob that has nothing to do with it: raising `extent` narrowed the
    /// buffer.
    ///
    /// Pinned in MVT units so the expected value is legible: the default 5 px
    /// (tippecanoe's `-b 5`) of a nominal 256 px tile is 80 units at extent
    /// 4096.
    #[test]
    fn tile_buffer_is_tile_pixels_not_extent_units() {
        let opts = ExportOptions::default();
        assert_eq!(opts.tile_buffer, 5);
        assert_eq!(opts.extent, 4096);

        let in_mvt_units = buffer_fraction(&opts) * f64::from(opts.extent);
        assert!(
            (in_mvt_units - 80.0).abs() < 1e-9,
            "5 tile pixels must be 80 MVT units at extent 4096, got {in_mvt_units}"
        );

        // Raising the extent must not change how wide the buffer is on the
        // ground — it only changes how finely that width is expressed.
        let finer = ExportOptions {
            extent: 8192,
            ..Default::default()
        };
        assert!(
            (buffer_fraction(&finer) - buffer_fraction(&opts)).abs() < 1e-12,
            "extent must not affect the buffer width"
        );
    }

    /// A feature just inside a neighbouring tile's edge buffer must be a member
    /// of that tile too, or the neighbour has nothing to draw the overlapping
    /// part from and the feature is sliced at the seam. `--tile-buffer` is
    /// documented as "carried across tile seams so features don't clip at
    /// boundaries", which only holds if buffer widens tile MEMBERSHIP and not
    /// just the clip rectangle of tiles the feature already occupies.
    ///
    /// The z1 seam is at lng 0, with tile (0,0) to its west and (1,0) to its
    /// east. A point just east of the seam is inside (0,0) once that tile is
    /// grown by its 8 px buffer, so both tiles must carry it. tippecanoe emits
    /// it in both. A point comfortably beyond the buffer must NOT be in (0,0) —
    /// without that half the assertion passes for any buffer at all, including
    /// a wildly too-wide one.
    #[test]
    fn point_in_neighbour_buffer_is_a_member_of_that_tile() {
        let opts = ExportOptions::default();
        let zoom = 1;
        // Buffer in degrees at z1: tile_width * buffer_px / 256
        // = 180 * 8 / 256 = 5.625°.
        let buffer_deg = buffer_deg_at_zoom(zoom, &opts);
        let west = tile_key(0, 0, zoom);
        let east = tile_key(1, 0, zoom);

        // Inside (0,0)'s buffer, and inside (1,0) proper.
        let inside = 0.9 * buffer_deg;
        let keys = members_by_key(feature_tile_members(
            &Geometry::Point(geo::Point::new(inside, 10.0)),
            zoom,
            &opts,
            0,
            u64::MAX,
            &crate::diagnostics::Diagnostics::default(),
        ));
        assert!(keys.contains_key(&east), "must be in its own tile (1,0)");
        assert!(
            keys.contains_key(&west),
            "point {inside}° is within tile (0,0)'s {buffer_deg}° buffer but was \
             not made a member of it; emitted tiles: {:?}",
            keys.keys().collect::<Vec<_>>()
        );

        // Beyond the buffer: (1,0) only. Pins the buffer's WIDTH, not just that
        // some widening happens.
        let outside = 1.5 * buffer_deg;
        let keys = members_by_key(feature_tile_members(
            &Geometry::Point(geo::Point::new(outside, 10.0)),
            zoom,
            &opts,
            0,
            u64::MAX,
            &crate::diagnostics::Diagnostics::default(),
        ));
        assert!(keys.contains_key(&east), "must be in its own tile (1,0)");
        assert!(
            !keys.contains_key(&west),
            "point {outside}° is {buffer_deg}° beyond tile (0,0) and must not be \
             a member of it; emitted tiles: {:?}",
            keys.keys().collect::<Vec<_>>()
        );
    }

    /// A bbox lying wholly outside the lon/lat domain must not come back from
    /// [`expand_bbox`] looking wrapped.
    ///
    /// Clamping each edge on its own crosses them over — `lng_min` floored at
    /// -180, `lng_max` capped at +180 — and `tile_ranges_for_bbox` reads
    /// `lng_min > lng_max` as an antimeridian crossing and returns EVERY tile
    /// column: 16,385 of them at z14, for a feature that belongs to none. A
    /// dataset on the 0..360 convention, or one stray Pacific point at lng
    /// 180.001, is enough. It fires at `--tile-buffer 0` too, so the defect is
    /// in the clamp rather than the buffer width.
    #[test]
    fn out_of_domain_bbox_does_not_claim_every_tile_column() {
        for tile_buffer in [0, 8, 128] {
            let opts = ExportOptions {
                tile_buffer,
                ..Default::default()
            };
            for zoom in [1u8, 10, 14] {
                for lng in [180.5, -180.5] {
                    let bbox = TileBounds::new(lng, -17.0, lng, -17.0);
                    let widened = expand_bbox(&bbox, buffer_deg_at_zoom(zoom, &opts));
                    assert!(
                        widened.lng_min <= widened.lng_max,
                        "expand_bbox manufactured a wrapped bbox at lng {lng}, z{zoom}, \
                         buffer {tile_buffer}: {widened:?}"
                    );
                    let ranges = member_ranges(&bbox, zoom, &opts);
                    assert!(
                        ranges.x2.is_none(),
                        "out-of-domain bbox at lng {lng}, z{zoom}, buffer {tile_buffer} \
                         took the antimeridian branch: {ranges:?}"
                    );
                    let span = u64::from(ranges.x.1 - ranges.x.0 + 1);
                    assert!(
                        span <= 2,
                        "out-of-domain bbox at lng {lng}, z{zoom}, buffer {tile_buffer} \
                         claims {span} tile columns"
                    );
                }
            }
        }
    }

    /// A bbox that arrives ALREADY wrapped must stay wrapped — the collapse
    /// above is only for a wrap the clamp itself manufactured.
    #[test]
    fn genuinely_wrapped_bbox_survives_expansion() {
        let opts = ExportOptions::default();
        let bbox = TileBounds::new(170.0, 0.0, -170.0, 10.0);
        let widened = expand_bbox(&bbox, buffer_deg_at_zoom(4, &opts));
        assert!(
            widened.lng_min > widened.lng_max,
            "a genuinely wrapped bbox must not be collapsed: {widened:?}"
        );
    }
}
