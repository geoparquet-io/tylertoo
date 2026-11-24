use super::*;

/// Resolve the Web Mercator zoom for overview level `level_idx`.
///
/// Uses the level's explicit `zoom` when present (§3.2). When absent, derives it
/// from the level GSD via the §5.2 inverse — `z = round(log2(C / base / gsd))`.
/// The rounding rule (nearest integer) is documented here so the mapping is
/// reproducible: a level whose GSD sits between two zooms maps to the nearer
/// one.
///
/// Fallible since #371: the derivation used to `clamp(0.0, 255.0)`, which let a
/// fine enough `--gsd` reach z255 with no error and no regard for `--max-zoom`.
/// A level above [`MAX_ZOOM`] is now rejected, naming the GSD and the zoom it
/// implies.
///
/// The degenerate GSDs are told apart rather than lumped into the ceiling
/// message (#371). A GSD of `0`, a negative one, or `NaN` is not a ground
/// sample distance at all — it has no zoom to be above or below a ceiling, so
/// it gets [`LevelGsdNotFinitePositive`] instead of being reported as "implies
/// zoom 9223372036854775807". `gsd = +inf` *is* meaningful: infinitely coarse,
/// which inverts to `z = -inf` and lands on z0, the same floor any too-coarse
/// level lands on. A finite but absurdly small GSD still inverts to `+inf` and
/// is reported, correctly, as above the ceiling.
///
/// [`LevelGsdNotFinitePositive`]: ExportError::LevelGsdNotFinitePositive
///
/// # Errors
///
/// Returns [`ExportError::LevelGsdNotFinitePositive`] for a GSD that is zero,
/// negative, or NaN, and [`ExportError::LevelZoomAboveCeiling`] for a level
/// whose zoom is above [`MAX_ZOOM`].
pub fn zoom_for_level(meta: &OverviewsMeta, level_idx: usize) -> Result<u8, ExportError> {
    let level = &meta.levels[level_idx];
    let implied: i64 = match level.zoom {
        Some(z) => i64::from(z),
        None => {
            if level.gsd.is_nan() || level.gsd <= 0.0 {
                return Err(ExportError::LevelGsdNotFinitePositive {
                    level: level_idx,
                    gsd: level.gsd,
                });
            }
            if level.gsd == f64::INFINITY {
                // Infinitely coarse: the whole world in one tile.
                return Ok(0);
            }
            let z = zoom_for_gsd(level.gsd).round();
            if z.is_infinite() {
                // A subnormal GSD: finite and positive, but so fine that the
                // §5.2 inverse overflows. Above the ceiling, by a lot.
                return Err(ExportError::LevelZoomAboveCeiling {
                    level: level_idx,
                    gsd: level.gsd,
                    implied: i64::MAX,
                    ceiling: MAX_ZOOM,
                });
            }
            z.clamp(i64::MIN as f64, i64::MAX as f64) as i64
        }
    };
    if implied > i64::from(MAX_ZOOM) {
        return Err(ExportError::LevelZoomAboveCeiling {
            level: level_idx,
            gsd: level.gsd,
            implied,
            ceiling: MAX_ZOOM,
        });
    }
    Ok(implied.max(0) as u8)
}

/// Every level's zoom, in level order, required to strictly ascend (#371 /
/// #506 follow-up — see the call site in `export_pmtiles_impl`). Pulled out
/// of it to keep that function under clippy's function-length ceiling.
pub(super) fn resolve_level_zooms(
    meta: &OverviewsMeta,
    num_levels: usize,
) -> Result<Vec<u8>, ExportError> {
    let level_zooms: Vec<u8> = (0..num_levels)
        .map(|level_idx| zoom_for_level(meta, level_idx))
        .collect::<Result<_, _>>()?;
    for i in 1..level_zooms.len() {
        if level_zooms[i] <= level_zooms[i - 1] {
            return Err(ExportError::LevelZoomsNotAscending {
                previous_level: i - 1,
                previous_zoom: level_zooms[i - 1],
                previous_gsd: meta.levels[i - 1].gsd,
                level: i,
                zoom: level_zooms[i],
                gsd: meta.levels[i].gsd,
            });
        }
    }
    Ok(level_zooms)
}

/// Members-per-partition target for the partitioned streaming export (H3(b)).
///
/// Each zoom's tiles are split into contiguous PMTiles tile-id ranges (#506;
/// row-major `(x, y)` ranges before it) whose summed (feature × tile) member
/// counts reach at least this value; partitions are processed one at a time
/// and streamed to the writer, so this bounds the per-partition working set
/// (clipped geometries + encoded tiles) instead of holding the whole zoom in
/// memory.
pub(super) const DEFAULT_PARTITION_TARGET: usize = 32_768;

/// The share of the partition target, in percent, that `bounded` plans a
/// level's partitions at once the level takes several waves (#815). A wave
/// keeps one partition per core, so its members shrink to this share while
/// every core still encodes one. Such a level already spills every wave but
/// the one it encodes, so smaller waves add no spill; a one-wave level keeps
/// its partitions, as splitting it would spill what it holds. The archive
/// does not depend on the target.
///
/// Smaller waves add wave barriers, where the pool idled on each wave's
/// largest tile (one tile encodes on one thread): a quarter cost NZ parcels
/// 9% of its wall in the serial wave loop. With the waves overlapped
/// ([`WAVE_SLACK_PERMILLE`]) the barrier is gone: NZ parcels
/// bounded 3,993 -> 2,909 MiB peak RSS and 74.3 -> 71.3 s wall (medians
/// of 4, quiet host).
pub(super) const BOUNDED_MULTI_WAVE_TARGET_PERCENT: usize = 25;

/// How far past its largest wave, in permille, a level group's members in
/// flight may run, so the next wave or level is collected and starts
/// encoding while the last partitions before it finish (#815). A bounded
/// multi-wave level is planned with this slack priced in ([`plan_level`]),
/// so the overlap stays inside the collect grant; under `auto` and `speed`
/// every group takes it, within the members [`MemoryPhase::ExportCollect`]
/// affords ([`level_group_member_cap`]). Slack 0, 12.5% and 50% gave the
/// same NZ wall; this is the middle.
pub(super) const WAVE_SLACK_PERMILLE: usize = 125;

/// `members` plus the [`WAVE_SLACK_PERMILLE`] slack on them.
pub(super) fn with_wave_slack(members: usize) -> usize {
    members.saturating_add(members.saturating_mul(WAVE_SLACK_PERMILLE) / 1000)
}

/// Sentinel for [`ExportOptions::partition_wave`] requesting automatic sizing.
///
/// The size comes from the machine's available parallelism (see
/// [`resolve_partition_wave`]). This is the library default so an export uses the cores it is given without
/// the caller having to know the box (#293).
pub const PARTITION_WAVE_AUTO: usize = 0;

/// Lower clamp for the auto-sized partition wave. Matches the historical
/// hard-coded width (#228): auto never schedules *narrower* than the tuned
/// baseline, so a small box behaves exactly as before.
pub const PARTITION_WAVE_MIN: usize = 6;

/// The run's memory budget, as export probes it (#788): the cached
/// container-aware figure every `auto` decision shares. Export's waves and
/// encodes are granted [`MemoryPhase::ExportCollect`]'s and
/// [`MemoryPhase::ExportEncode`]'s share of it, half of the probed figure,
/// below the allowance because the waves sit on top of export state no
/// estimate models (scan accumulators, decode buffers, the writer's
/// directory). Bias is toward not `OOMing`: a too-narrow wave only costs
/// wall clock.
pub(super) fn export_budget() -> MemoryBudget {
    MemoryBudget::with_available(available_memory_bytes())
}

/// The bytes [`MemoryPhase::ExportCollect`] may hold under `budget` and
/// `profile`, for the decision logs.
pub(super) fn export_collect_bytes(profile: MemoryProfile, budget: &MemoryBudget) -> u64 {
    budget.affordable(profile, MemoryPhase::ExportCollect, Price::model(1)) as u64
}

/// Flat fallback estimate of one wave slot's transient memory cost, used **only
/// by the upfront ceiling** [`memory_safe_wave`] and when the data-aware
/// per-level estimate ([`memory_safe_wave_members`], #311) cannot be computed
/// (finest-level row size unknown).
///
/// Measured (2026-07, #293 benchmark, germany-segments 19.2M-feature z14
/// band): widening 6 → 16 lifted peak RSS 588 → 848 MiB, i.e. ~26 MiB per
/// wave slot. It was calibrated on **lines** and does **not** generalize: on
/// dense finest-zoom polygon data a single dense tile is one partition holding
/// millions of members, so the true per-slot transient runs ~120× this (#311,
/// Brazil field boundaries: ~7.5 GiB/slot). That is why the binding memory
/// guard is now the per-level [`memory_safe_wave_members`], which caps the
/// members a wave actually holds ([`cut_waves`], #876); this constant survives only
/// as the coarse upfront ceiling and the last-resort fallback.
pub(super) const PARTITION_SLOT_TRANSIENT_BYTES: u64 = 64 * 1024 * 1024;

/// In-memory inflation applied to the finest level's mean **stored**
/// (uncompressed Parquet) bytes/row to estimate a resident clipped **member**'s
/// cost (#311).
///
/// A member holds a decoded `geo::Geometry<f64>` (16 B/coord, no column
/// compression, `Vec` capacity slack) and is briefly co-resident with its
/// gzip-encoded MVT copy in the same wave, so 2× the stored size is a
/// deliberately high bias — which is what we want, since a too-narrow wave only
/// costs wall clock while an OOM loses the whole run.
pub(super) const MEMBER_MEMORY_INFLATION: u64 = 2;

/// Resident bytes of one clipped member beyond its inflated stored bytes,
/// before any property (#665): the `Member` struct (128 B), allocator slack,
/// and its share of the full-tile MVT build in [`build_mvt`].
///
/// The stored-bytes term alone under-counted points about 5×. Measured
/// (2026-11-16, release build, aarch64, `export-pmtiles` of a z0 overview
/// holding every one of 200k and 2M world-spread points, no tile caps, peak
/// RSS slope between the two sizes), in bytes per row stored in the overview
/// and per member resident. Properties are packed row buffers (#874), so
/// they cost their bytes, not a slot, a string and tag entries each:
///
/// | shape | stored | properties | measured | estimate |
/// |---|---|---|---|---|
/// | Overture-shaped, four properties | 115 | 4 | 618 | 934 |
/// | the same, `--exclude-all-properties` | 115 | 0 | 460 | 454 |
/// | geometry only | 57 | 0 | 432 | 338 |
/// | four numeric | 96 | 4 | 879 | 896 |
/// | sixteen numeric | 213 | 16 | 2,334 | 2,570 |
/// | four strings, one a 32-character id | 107 | 4 | 647 | 918 |
///
/// [`MEMBER_PROPERTY_BYTES`] is the smallest round value that covers every
/// row with properties: four numeric sets it. Before packed rows the same
/// shapes measured 843, 483, 448, 1,061, 2,857 and 815 on this host, and
/// the price was 208. This constant is unchanged from the old layout and
/// does not cover the two property-less rows on this host: the old layout
/// missed them too (483 and 448 against 460 and 432). Raising it to 336
/// would cover them and would narrow dense property-less levels' waves.
pub(super) const MEMBER_BASE_BYTES: u64 = 224;

/// Resident bytes each published property adds to one member (#665, #874):
/// its tag, length and payload in the packed row, plus the MVT tag entries.
/// Measured at 80–110 B each, numeric or string; see [`MEMBER_BASE_BYTES`]
/// for the fit.
pub(super) const MEMBER_PROPERTY_BYTES: u64 = 120;

/// Extra transient bytes per member when the size valve can merge: grouping,
/// quantized polygons, interactions and union work alongside collected members.
///
/// Fitted (2026-11, dhat, debug build, two workers, 100k overlapping members
/// per fixture, inputs allocated before profiling) on both 5-vertex squares
/// and 40-vertex jittered-radius rings, including each ring's closing vertex.
/// #908 added the part-join cache, the lock-free union-find and per-part
/// validity flags (about 20 B/member on vertex-heavy, highest of three runs):
///
/// | fixture | peak live bytes | bytes/member |
/// |---|---|---|
/// | squares | 26,829,040 | 268.29 |
/// | vertex-heavy | 58,584,390 | 585.84 |
///
/// With the separately charged 8 MiB gzip cache, 400 B/member estimates
/// 48,388,608 B: measured/estimated is 0.55× and 1.21× respectively. Refit
/// with the process-isolated `valve_encode_peak_live_bytes_matches_price`
/// ledgers; their 0.5–1.25× bound guards both underpricing and waste
/// returning. Drop/unlimited add nothing.
pub(super) const VALVE_TRANSIENT_BYTES_PER_MEMBER: u64 = 400;

/// The shared wave/group/store fit price, including merge-valve workspace.
pub(super) fn export_member_resident_bytes(resident: u64, opts: &ExportOptions) -> u64 {
    resident.saturating_add(if valve_merges(opts) {
        VALVE_TRANSIENT_BYTES_PER_MEMBER
    } else {
        0
    })
}

/// Estimated resident bytes of one clipped member (#311, #665): the finest
/// level's mean stored bytes per row times [`MEMBER_MEMORY_INFLATION`], plus
/// [`MEMBER_BASE_BYTES`] and [`MEMBER_PROPERTY_BYTES`] per published
/// property. It over-estimates every measured shape with properties, by up
/// to 51%; for property-less members it falls short by up to 22% (they measure
/// up to 28% above it; see [`MEMBER_BASE_BYTES`]).
pub(super) fn member_resident_bytes(mean_stored_bytes: u64, properties: usize) -> u64 {
    mean_stored_bytes
        .saturating_mul(MEMBER_MEMORY_INFLATION)
        .saturating_add(MEMBER_BASE_BYTES)
        .saturating_add((properties as u64).saturating_mul(MEMBER_PROPERTY_BYTES))
}

/// The widest wave the memory budget allows under `profile`: how many
/// [`PARTITION_SLOT_TRANSIENT_BYTES`] slots [`MemoryPhase::ExportCollect`]
/// can afford (a model price: a flat calibration). With RAM unknown the
/// budget's fallback allowance prices it, as every other phase's; under
/// `speed` there is no cap (#788).
pub(super) fn memory_safe_wave(profile: MemoryProfile, budget: &MemoryBudget) -> usize {
    budget.affordable(
        profile,
        MemoryPhase::ExportCollect,
        Price::model(PARTITION_SLOT_TRANSIENT_BYTES),
    )
}

/// The auto partition-wave decision (#303): pure so it is unit-testable
/// across the core-count × RAM grid, mirroring #294's `auto_backing`.
///
/// `max(PARTITION_WAVE_MIN, min(cores, memory_safe_wave))` — cores are the
/// useful upper bound (a wave wider than the core count gains nothing), the
/// memory-safe wave keeps the transient inside the RAM budget, and the floor
/// preserves the historical width-6 baseline (its worst-case transient is
/// small and known-safe, and narrower waves only lose throughput).
pub(super) fn auto_partition_wave(
    cores: usize,
    profile: MemoryProfile,
    budget: &MemoryBudget,
) -> usize {
    cores
        .min(memory_safe_wave(profile, budget))
        .max(PARTITION_WAVE_MIN)
}

/// The most members one wave of a level may hold (#311, #876), the fix for
/// the flat [`PARTITION_SLOT_TRANSIENT_BYTES`] under-count that OOM-killed
/// dense exports on `auto`.
///
/// [`cut_waves`] sums the members a wave actually holds against this cap, so
/// one dense partition (a tile that overshoots [`DEFAULT_PARTITION_TARGET`]
/// is one partition, unbounded by the target) narrows only its own wave.
/// Pricing every slot at the densest partition narrowed whole levels: on the
/// #908 input z10 ran at wave 15 of 64, 46% slower, for no memory it needed.
/// A wave may still fall to one partition, below [`PARTITION_WAVE_MIN`]:
/// a serial wave that finishes beats an OOM that never does.
///
/// No cap (`usize::MAX`) when the per-member size is unknown (empty finest
/// level), where the flat preflight already governed. With RAM unknown the
/// budget's fallback allowance prices it; under `speed` there is no cap
/// (#788).
pub(super) fn memory_safe_wave_members(
    member_bytes: Option<u64>,
    profile: MemoryProfile,
    budget: &MemoryBudget,
) -> usize {
    // A footer price: the finest level's mean stored row bytes from the
    // footer, inflated with the optional valve workspace by
    // [`export_member_resident_bytes`].
    member_bytes
        .filter(|&b| b > 0)
        .map_or(usize::MAX, |per_member| {
            budget.affordable(
                profile,
                MemoryPhase::ExportCollect,
                Price::new(per_member, PriceBasis::Footer),
            )
        })
}

/// The most members one concurrent level group (#647) may hold at once.
///
/// A serial export holds at most one wave of partitions. One full wave at
/// the ceiling width, each partition at the planned target, is
/// `ceiling × partition_target` members, so a group's collected members
/// stay within what the serial loop already budgets for. When the member
/// size is known, the cap also narrows to the members
/// [`MemoryPhase::ExportCollect`] can afford under `profile` at
/// `member_bytes` each (see [`export_member_resident_bytes`]), the same estimate
/// [`memory_safe_wave_members`] uses; under `speed` it does not narrow.
///
/// The cap counts members only. A group can also hold the encoded tiles of
/// a level that finished before a coarser one, until [`GroupCommit`] writes
/// it. Encoding consumes a partition's members, and gzipped MVT bytes are
/// much smaller than the members they come from, so this stays below the
/// members' own size. The group reads one level at a time, as the serial
/// loop does, so at most one band reader is open.
pub(super) fn level_group_member_cap(
    ceiling_wave: usize,
    partition_target: usize,
    member_bytes: Option<u64>,
    profile: MemoryProfile,
    budget: Option<&MemoryBudget>,
) -> usize {
    let wave_cap = ceiling_wave.saturating_mul(partition_target);
    let (Some(per_member), Some(budget)) = (member_bytes.filter(|&b| b > 0), budget) else {
        return wave_cap;
    };
    wave_cap.min(budget.affordable(
        profile,
        MemoryPhase::ExportCollect,
        Price::new(per_member, PriceBasis::Footer),
    ))
}

/// The error for a level whose densest tile cannot fit in `limit` even when
/// it is the only one in flight (#665), or `None` when it fits.
///
/// [`cut_waves`] can narrow a wave down to one partition, but no
/// further, and [`plan_partitions`] never splits a tile. With thinning off
/// (`--verbatim`), the z0 tile holds every feature. Exporting a tile means
/// holding all its members and then their full MVT, so a tile estimated past
/// all of the machine's memory would only OOM, after minutes of work.
///
/// It prices the densest tile, not the densest partition. A partition adds
/// whole tiles until it reaches its member target, so it exceeds its densest
/// tile by less than one target's worth of members. But
/// [`member_resident_bytes`] prices every member at a whole stored row, and
/// the members of a partition of many small tiles are often clipped pieces
/// of large polygons, each far smaller than its row. Such a partition is left
/// to the wave guard, as before #665.
///
/// The threshold is the whole figure, not [`MemoryPhase::ExportCollect`]'s
/// the wave budget uses: this refuses only what cannot fit serially. First of
/// equally dense tiles, so the message is deterministic.
pub(super) fn densest_tile_over_limit(
    level: usize,
    zoom: u8,
    tile_counts: &BTreeMap<u64, usize>,
    per_member: u64,
    limit: MemoryLimit,
) -> Option<ExportError> {
    let (key, members) =
        tile_counts
            .iter()
            .fold(None, |best: Option<(u64, usize)>, (&k, &c)| match best {
                Some((_, b)) if b >= c => best,
                _ => Some((k, c)),
            })?;
    let estimate = (members as u64).saturating_mul(per_member);
    if estimate <= limit.bytes {
        return None;
    }
    let (x, y) = key_to_xy(key, zoom);
    Some(ExportError::TileExceedsMemory {
        level,
        zoom,
        x,
        y,
        members,
        estimate_bytes: estimate,
        available_bytes: limit.bytes,
        limit_source: limit.source.describe(),
    })
}

/// Refuse the export, after the scan and before the first tile, when a
/// level's densest tile cannot fit in memory on its own (#665; see
/// [`densest_tile_over_limit`]). Runs under any `--partition-wave`, since no
/// wave makes a tile smaller.
///
/// `limit` comes from [`probe_oom_memory_limit`]. Unknown member size or
/// memory never refuses, matching the wave guard's fallbacks. Two figures
/// only warn and continue, as the #543 pass-1 preflight does:
///
/// - the `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override, a knob for making `auto`
///   conservative (the docs suggest setting it low to cut convert's peak),
///   not a statement about the machine;
/// - any figure while `skip` ([`SKIP_MEMORY_PREFLIGHT_ENV`]) is set.
///
/// DIVERGENCE FROM #543: that preflight refuses only against a cgroup
/// `memory.max`, since its row-count floor comes before the scan. This one
/// also refuses against `MemAvailable`, because the member count is exact
/// and the whole tile must be resident at once. The #665 run had no cgroup
/// limit, so `memory.max` alone would not have stopped it.
pub(super) fn check_levels_fit(
    plans: &[LevelPlan],
    scans: &[LevelScan],
    member_bytes: Option<u64>,
    limit: Option<MemoryLimit>,
    skip: bool,
) -> Result<(), ExportError> {
    // A zero member size needs no filter: it never estimates past a figure.
    let (Some(per_member), Some(limit)) = (member_bytes, limit) else {
        return Ok(());
    };
    for (level, (plan, scan)) in plans.iter().zip(scans).enumerate() {
        let Some(err) =
            densest_tile_over_limit(level, plan.zoom, &scan.tile_counts, per_member, limit)
        else {
            continue;
        };
        let why = if limit.source == MemoryLimitSource::Override {
            "the memory figure is the TYLERTOO_AUTO_MEM_LIMIT_BYTES override".to_string()
        } else if skip {
            format!("{SKIP_MEMORY_PREFLIGHT_ENV} is set")
        } else {
            return Err(err);
        };
        log::warn!(target: LOG_TARGET, "[export] continuing because {why}, but it may be OOM-killed: {err}");
    }
    Ok(())
}

/// Resolve a requested partition-wave width to a concrete value.
///
/// [`PARTITION_WAVE_AUTO`] (0) runs the memory-budget preflight (#303): the
/// width is the machine's [`std::thread::available_parallelism`], capped by
/// how many per-partition transients (estimated
/// `PARTITION_SLOT_TRANSIENT_BYTES` each) fit in
/// the export collection phase's share of available RAM (probe shared with the
/// convert-side `--profile auto`: `TYLERTOO_AUTO_MEM_LIMIT_BYTES` override,
/// then the container-aware `min(cgroup headroom, /proc/meminfo
/// MemAvailable)` — see `available_memory_bytes`), floored at
/// [`PARTITION_WAVE_MIN`].
/// When RAM cannot be probed the budget's fallback allowance prices the
/// cap. This entry point sizes for the default `auto` profile. Any
/// explicit positive value is honoured verbatim (uncapped — the caller opted
/// in to the memory cost).
///
/// The output is a **scheduling** parameter only: it sets how many partitions
/// are processed per band read, never which features land in which tile or the
/// order tiles reach the writer. Every wave width therefore produces a
/// byte-identical archive (pinned by `export_wave_width_is_byte_invariant` and
/// the partition-invariance / frozen-hash tests).
pub fn resolve_partition_wave(requested: usize) -> usize {
    resolve_partition_wave_under(requested, MemoryProfile::Auto, &export_budget())
}

/// [`resolve_partition_wave`] under the run's `profile` and `budget`.
pub(super) fn resolve_partition_wave_under(
    requested: usize,
    profile: MemoryProfile,
    budget: &MemoryBudget,
) -> usize {
    if requested == PARTITION_WAVE_AUTO {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(PARTITION_WAVE_MIN);
        auto_partition_wave(cores, profile, budget)
    } else {
        requested
    }
}

/// Resolve the partition-wave width (running the #303 memory preflight when
/// the caller left it at [`PARTITION_WAVE_AUTO`]) and log the chosen width
/// alongside the inputs that produced it (detected cores, memory-safe cap,
/// available RAM) once at export start, so both export core utilization
/// (#293) and the preflight decision (#303) are observable — mirrors the
/// `[convert] pass2 auto` decision log line.
pub(super) fn resolve_and_log_partition_wave(
    requested: usize,
    profile: MemoryProfile,
    budget: &MemoryBudget,
) -> usize {
    let wave = resolve_partition_wave_under(requested, profile, budget);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    if requested == PARTITION_WAVE_AUTO {
        let avail_str = budget.available().map_or_else(
            || "unknown".to_string(),
            |b| format!("{} MiB", b / (1024 * 1024)),
        );
        let cap = match memory_safe_wave(profile, budget) {
            usize::MAX => "unlimited".to_string(),
            cap => cap.to_string(),
        };
        log::info!(target: LOG_TARGET,
            "[export] partition wave: {wave} partition(s) per band read \
             (auto: {cores} core(s), memory-safe cap {cap} under {}, avail {avail_str})",
            profile.as_str(),
        );
    } else {
        log::info!(target: LOG_TARGET,
            "[export] partition wave: {wave} partition(s) per band read \
             (explicit; {cores} core(s) detected)"
        );
    }
    wave
}

/// Plan every level's partitions and wave width.
///
/// `ceiling_wave` is the resolved `--partition-wave` upper bound. On `auto`
/// each level narrows it further from its own densest partition (#311); an
/// explicit wave is honoured verbatim. `member_bytes` includes merge-valve
/// workspace; `fit_member_bytes` keeps the historical refusal price from
/// [`member_resident_bytes`]. Under any wave, a level whose densest tile
/// cannot fit in `fit_limit` on its own is refused
/// ([`check_levels_fit`], #665). `fit_limit` is probed fresh rather than
/// taken from the cached `available_ram`: in a `tiles` run that figure was
/// fixed during convert's pass 1.
pub(super) fn plan_levels(
    scans: &[LevelScan],
    level_zooms: &[u8],
    inputs: &PartitionBudget,
) -> Result<Vec<LevelPlan>, ExportError> {
    let num_levels = scans.len();
    let plans: Vec<LevelPlan> = scans
        .iter()
        .zip(level_zooms)
        .enumerate()
        .map(|(level_idx, (scan, &zoom))| {
            plan_level(&scan.tile_counts, zoom, inputs, || {
                format!("level {}/{num_levels} z{zoom}", level_idx + 1)
            })
        })
        .collect();
    check_levels_fit(
        &plans,
        scans,
        inputs.fit_member_bytes,
        inputs.fit_limit,
        skip_memory_preflight_from_env(),
    )?;
    Ok(plans)
}

/// One zoom's partitions and wave width: its tiles split into contiguous
/// ascending PMTiles tile-id ranges (#506) of about `partition_target`
/// members each, and on `auto` the wave narrowed from the densest of them
/// (#311). `label` names the zoom in the guard's log line.
pub(super) fn plan_level(
    tile_counts: &BTreeMap<u64, usize>,
    zoom: u8,
    inputs: &PartitionBudget,
    label: impl Fn() -> String,
) -> LevelPlan {
    let PartitionBudget {
        partition_target,
        ceiling_wave,
        auto_wave,
        member_bytes,
        profile,
        ref budget,
        ..
    } = *inputs;
    let plan = |target: usize, member_bytes: Option<u64>| {
        let partitions = plan_partitions(tile_counts, zoom, target);
        let cap = if auto_wave {
            memory_safe_wave_members(member_bytes, profile, budget)
        } else {
            usize::MAX
        };
        LevelPlan::cut(zoom, partitions, ceiling_wave, cap)
    };
    // Per-level memory guard (#311): on `auto`, each wave holds only the
    // members the budget affords, so a dense finest zoom does not OOM the
    // whole run (#876: it narrows only the waves that hold dense
    // partitions). Explicit waves are honoured verbatim.
    let mut level = plan(partition_target, member_bytes);
    if profile == MemoryProfile::Bounded && level.wave_count() > 1 {
        // The waves overlap by the slack ([`group_schedule`]): priced in,
        // a wave and its slack fit the grant one wave did.
        let slack_bytes = member_bytes.map(|b| with_wave_slack(b as usize) as u64);
        level = plan(
            partition_target * BOUNDED_MULTI_WAVE_TARGET_PERCENT / 100,
            slack_bytes,
        );
    }
    if level.narrowed {
        let densest = level
            .partitions
            .iter()
            .map(|p| p.members)
            .max()
            .unwrap_or(0);
        let narrowest = level.waves().map(<[Partition]>::len).min().unwrap_or(0);
        log::info!(target: LOG_TARGET,
            "[export] {}: {} waves of {narrowest}–{} partitions, ceiling {ceiling_wave} \
             (densest partition {densest} members × ~{} B/member resident, {} MiB budget) \
             — #311 density guard",
            label(),
            level.wave_count(),
            level.widest_wave(),
            member_bytes.unwrap_or(0),
            export_collect_bytes(profile, budget) / (1024 * 1024),
        );
    }
    level
}

/// #541 review: a partial overview (written under a convert-side zoom
/// ceiling) holds only the levels at or coarser than that ceiling, and its
/// finest level is NOT the canonical one. Exporting it as if it were a whole
/// pyramid would silently ship a truncated archive; only an export that
/// itself stops at or before the recorded ceiling is what the file was built
/// for.
///
/// Returns the finest zoom the export emits: the file's finest level's zoom,
/// capped by the export's own ceiling.
pub(super) fn emitted_max_zoom(
    meta: &OverviewsMeta,
    options: &ExportOptions,
    finest_level_zoom: u8,
) -> Result<u8, ExportError> {
    if let Some(recorded) = meta.generalization.as_ref().and_then(|g| g.zoom_ceiling) {
        if options.zoom_ceiling.is_none_or(|c| c > recorded) {
            return Err(ExportError::PartialOverview {
                recorded,
                requested: options.zoom_ceiling,
            });
        }
    }
    Ok(match options.zoom_ceiling {
        Some(c) => finest_level_zoom.min(c),
        None => finest_level_zoom,
    })
}
