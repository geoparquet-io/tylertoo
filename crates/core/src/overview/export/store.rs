use super::*;

/// A member's geometry: its own, or the feature's decoded geometry shared by
/// every level whose tile holds the feature unclipped (#815: the partitioning
/// fill held one copy per level). Reads as a plain [`Geometry`].
pub(super) enum MemberGeom {
    Owned(Geometry<f64>),
    Shared(Arc<Geometry<f64>>),
}

impl std::ops::Deref for MemberGeom {
    type Target = Geometry<f64>;

    fn deref(&self) -> &Geometry<f64> {
        match self {
            MemberGeom::Owned(g) => g,
            MemberGeom::Shared(g) => g,
        }
    }
}

/// A feature's share of one tile: the feature whole, when the tile's
/// buffered bounds hold it, or its clip.
pub(super) enum TilePiece {
    Whole,
    Clipped(Geometry<f64>),
}

impl From<Geometry<f64>> for MemberGeom {
    fn from(g: Geometry<f64>) -> Self {
        MemberGeom::Owned(g)
    }
}

/// One tile member: a feature's clipped geometry destined for the tile with
/// key `key`, the feature's band-order sequence number (stable within-tile
/// ordering), the feature's MVT properties (shared across the feature's tile
/// copies), and its resolved `--feature-id` (#443), if any -- also shared
/// across every tile/zoom copy of the feature, which is what makes the id
/// stable across tile and zoom boundaries.
pub(super) struct Member {
    pub(super) key: u64,
    pub(super) seq: u64,
    /// The feature's point-thinning rank (#768, [`point_rank`]): the same in
    /// every tile and zoom that holds the feature, so a drop that keeps the
    /// best-ranked points nests across zooms and agrees across tile edges.
    pub(super) rank: u64,
    pub(super) geom: MemberGeom,
    pub(super) props: SharedProps,
    /// `Some` on every member when [`ExportOptions::feature_id`] is set
    /// (every row of the file validated up front, #443); `None` when it is
    /// not, in which case the MVT id is omitted.
    pub(super) id: Option<u64>,
    /// The feature's density gap (#745): how far it lies from the row before
    /// it, in squared Web Mercator world units. The size valve merges the
    /// densest members (smallest gap) first. [`GAP_NONE`] when the row has no
    /// predecessor in its row group. Computed once per row on the unclipped
    /// geometry, so every tile copy of a feature carries the same value. See
    /// [`coalesce_valve`].
    pub(super) gap: u64,
    /// The level band the row was read from. In partitioning mode a tile
    /// holds several bands, and the valve merges only within one.
    pub(super) band: u16,
}

/// One level's result from the scan pass ([`scan_all_levels`]).
#[cfg_attr(test, derive(Debug, PartialEq))]
pub(super) struct LevelScan {
    /// Rows in the level band.
    pub(super) feature_count: usize,
    /// Union of feature bboxes (`None` when no feature has one).
    pub(super) bounds: Option<TileBounds>,
    /// Member count per tile key, ascending PMTiles tile id (#506; ascending
    /// `(x, y)` before it).
    pub(super) tile_counts: BTreeMap<u64, usize>,
}

/// A contiguous range of tile keys, processed (and written) as one unit.
pub(super) struct Partition {
    pub(super) key_lo: u64,
    pub(super) key_hi: u64,
    /// The partition's tile bounds plus a one-tile margin, used to prune row
    /// groups on the partition's read pass. Pruning is conservative: a
    /// feature assigned to a tile in this partition has a bbox intersecting
    /// that tile's bounds, hence intersecting this region.
    pub(super) bbox: PruneRegion,
    /// Summed member count (for balancing).
    pub(super) members: usize,
}

/// The region a read prunes row groups to: the union of tile bounds west
/// of the prime meridian and the union east of it, kept apart. A partition
/// whose tiles sit on both sides of the antimeridian (the Chatham Islands
/// and the NZ mainland, adjacent in tile order) is then two narrow
/// rectangles, where one union rectangle spans every longitude and selects
/// every row group at those latitudes. Splitting at the prime meridian
/// instead of at ±180 handles both: a region across the prime meridian is
/// two rectangles that meet, covering what its union did.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PruneRegion {
    pub(super) west: Option<TileBounds>,
    pub(super) east: Option<TileBounds>,
}

impl PruneRegion {
    pub(super) fn of(bounds: TileBounds) -> Self {
        let mut region = PruneRegion {
            west: None,
            east: None,
        };
        region.add(&bounds);
        region
    }
    /// Add one tile's bounds to the side it lies on. A tile lies on one
    /// side at every zoom but 0, whose one tile goes east.
    pub(super) fn add(&mut self, bounds: &TileBounds) {
        let side = if bounds.lng_max <= 0.0 {
            &mut self.west
        } else {
            &mut self.east
        };
        match side {
            Some(b) => b.expand(bounds),
            None => *side = Some(*bounds),
        }
    }
    pub(super) fn expand(&mut self, other: &PruneRegion) {
        for b in other.rects() {
            self.add(&b);
        }
    }
    /// Grow each rectangle by `margin` degrees on every side.
    pub(super) fn grow(&mut self, margin: f64) {
        for b in [&mut self.west, &mut self.east].into_iter().flatten() {
            *b = TileBounds::new(
                b.lng_min - margin,
                b.lat_min - margin,
                b.lng_max + margin,
                b.lat_max + margin,
            );
        }
    }
    pub(super) fn rects(&self) -> impl Iterator<Item = TileBounds> {
        self.west.into_iter().chain(self.east)
    }
}

/// Shared per-level context for partition processing.
/// Deterministic work counts the #788 export tests pin (test builds only):
/// what the duplicating-mode wave collect does, independent of timing.
#[cfg(test)]
#[derive(Debug, Default, Clone)]
pub(super) struct ExportWorkTally {
    /// Rows the wave reads delivered, per level index, summed over every
    /// wave of the level (#740). A level whose band is read once has exactly
    /// its band's row count here; each wave that re-reads row groups another
    /// wave already read adds them again.
    pub(super) band_rows_read: BTreeMap<usize, u64>,
    /// Rows whose per-row collect work (geometry decode, property
    /// materialization and member routing in [`finish_group`]) ran
    /// on the wave's consumer thread, outside the Rayon pool, so that no
    /// other core could share it (#740).
    pub(super) serial_collect_rows: u64,
    /// Bytes each multi-wave level's one-level member store spilled (#740).
    pub(super) level_spill_bytes: BTreeMap<usize, u64>,
    /// Read batches [`finish_group`] took, and the calls (each a
    /// set of pool joins) it took them in (#817).
    pub(super) collect_batches: u64,
    pub(super) collect_groups: u64,
    /// Band batches read on the thread that collects them, which then
    /// waits on the read (#817).
    pub(super) band_reads_on_consumer: u64,
    /// Groups clipped while an earlier group still waited to finish (#817).
    pub(super) collect_overlaps: u64,
    /// The most groups one clip-ahead collect held at once (#876).
    pub(super) collect_max_held: usize,
    /// Waves of multi-wave levels a level group collected and encoded in
    /// its pipeline, not the serial wave loop (#817).
    pub(super) group_waves: u64,
    /// [`Pass2Knobs::hold_for_fill`].
    pub(super) hold_for_fill: bool,
    /// Level indices whose one-level store has started to fill.
    pub(super) fills_started: std::collections::BTreeSet<usize>,
    /// Held last waves that saw the next level's store start to fill before
    /// they encoded.
    pub(super) fills_beside_encode: u64,
    /// Per one-level store fill: the members in flight and the units of the
    /// wave grant sized for them.
    pub(super) fill_grants: Vec<(usize, usize)>,
    /// [`Pass2Knobs::stall_first_wave`].
    pub(super) stall_first_wave: bool,
    /// The most encoded waves a level group held unwritten at once.
    pub(super) peak_queued_waves: usize,
}

pub(super) struct LevelCtx<'a> {
    pub(super) reader: &'a OverviewReader,
    /// How the band is read: [`read_band`].
    pub(super) band: BandRead<'a>,
    pub(super) level_idx: usize,
    pub(super) crs: Crs,
    pub(super) zoom: u8,
    pub(super) opts: &'a ExportOptions,
    pub(super) gzip_cache: &'a Arc<TileGzipCache>,
    /// MVT key per schema column (#359); identity unless the file records a
    /// reserved-column rename whose source name is free again.
    pub(super) published: &'a PublishedNames,
    /// The resolved `--feature-id` column (#443), if any.
    pub(super) feature_id: Option<&'a ResolvedFeatureId>,
    /// Profiling stage timers (#535); diagnostics only.
    pub(super) timers: &'a ExportTimers,
    /// The #750 coverage fill, when asked for.
    pub(super) fills: Option<&'a coverage_fill::FillMap>,
    /// #803: the only tiles to clip into. A partition's key range can span
    /// a stopped tile, whose members must not be drawn again. `None` draws
    /// every tile in the range, which then holds only planned tiles.
    pub(super) tiles: Option<&'a BTreeMap<u64, usize>>,
    /// #802: the finest level's junction pins ([`finest_level_pins`]).
    pub(super) pins: Option<&'a JunctionNodes>,
    /// #768: the valve floors, with `--valve-scope smooth|zoom`.
    pub(super) floors: Option<&'a valve_predict::ValveFloors>,
    /// The export's test-only work counts (#788).
    #[cfg(test)]
    pub(super) work: &'a Mutex<ExportWorkTally>,
}

/// The per-export constants the single-read fan-out reads for every batch:
/// the file CRS, the level plans, the caller's export options, and the MVT
/// key mapping (#359). Grouped because they are fixed for the whole fill —
/// only the batch, its band, and the running sequence/sink vary. Mirrors
/// [`LevelCtx`] for the legacy per-wave path.
#[derive(Clone, Copy)]
pub(super) struct FanoutCtx<'a> {
    /// How the bands are read ([`send_bands`]).
    pub(super) band: BandRead<'a>,
    pub(super) crs: Crs,
    pub(super) plans: &'a [LevelPlan],
    pub(super) opts: &'a ExportOptions,
    pub(super) published: &'a PublishedNames,
    /// The resolved `--feature-id` column (#443), if any.
    pub(super) feature_id: Option<&'a ResolvedFeatureId>,
    /// Profiling stage timers (#535); diagnostics only.
    pub(super) timers: &'a ExportTimers,
    /// #802: the finest level's junction pins ([`finest_level_pins`]).
    pub(super) pins: Option<&'a JunctionNodes>,
}

/// One level's precomputed pass-2 plan: its Web Mercator zoom, its planned
/// partitions, and its resolved wave width. Computed once before pass 2 so the
/// partitioning single-read fill (#235) and the drain loop agree exactly on
/// partition boundaries and wave chunking.
pub(super) struct LevelPlan {
    pub(super) zoom: u8,
    pub(super) partitions: Vec<Partition>,
    /// Where each wave starts in `partitions`, ascending from 0 (#876): a
    /// wave holds the partitions up to the next start. Empty for an empty
    /// level. Waves are uneven when a dense partition narrows its own.
    pub(super) wave_starts: Vec<usize>,
    /// The member cap, not the ceiling, closed at least one wave: the #311
    /// density guard changed this level's waves.
    pub(super) narrowed: bool,
}

impl LevelPlan {
    /// `partitions` cut into waves of at most `ceiling` partitions and
    /// `member_cap` members ([`cut_waves`]).
    pub(super) fn cut(
        zoom: u8,
        partitions: Vec<Partition>,
        ceiling: usize,
        member_cap: usize,
    ) -> Self {
        let (wave_starts, narrowed) = cut_waves(&partitions, ceiling, member_cap);
        Self {
            zoom,
            partitions,
            wave_starts,
            narrowed,
        }
    }

    /// Wave `w`'s index range in `partitions`; empty past the last wave.
    pub(super) fn wave_range(&self, w: usize) -> std::ops::Range<usize> {
        let Some(&start) = self.wave_starts.get(w) else {
            return 0..0;
        };
        start
            ..self
                .wave_starts
                .get(w + 1)
                .copied()
                .unwrap_or(self.partitions.len())
    }

    /// Each wave's index range in `partitions`, in order.
    pub(super) fn wave_ranges(&self) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
        (0..self.wave_count()).map(|w| self.wave_range(w))
    }

    /// Wave `w`, in constant time; empty past the last wave (an empty
    /// level's one synthetic wave).
    pub(super) fn wave(&self, w: usize) -> &[Partition] {
        &self.partitions[self.wave_range(w)]
    }

    /// The level's waves, in partition (and so tile) order.
    pub(super) fn waves(&self) -> impl Iterator<Item = &[Partition]> + '_ {
        self.wave_ranges().map(|r| &self.partitions[r])
    }

    pub(super) fn wave_count(&self) -> usize {
        self.wave_starts.len()
    }

    /// The wave that holds partition `pidx`.
    pub(super) fn wave_of(&self, pidx: usize) -> usize {
        self.wave_starts
            .partition_point(|&s| s <= pidx)
            .saturating_sub(1)
    }

    /// The most partitions any one wave holds; 0 for an empty level.
    pub(super) fn widest_wave(&self) -> usize {
        self.waves().map(<[Partition]>::len).max().unwrap_or(0)
    }
}

/// Wave starts for `partitions` in order (#876): a new wave starts when the
/// current one holds `ceiling` partitions or the next partition would take
/// it past `member_cap` members. A partition over the cap alone is a wave of
/// its own. Order is unchanged, so every cut gives the same archive. Also
/// returns whether the cap, not the ceiling, closed any wave.
pub(super) fn cut_waves(
    partitions: &[Partition],
    ceiling: usize,
    member_cap: usize,
) -> (Vec<usize>, bool) {
    let ceiling = ceiling.max(1);
    let mut starts = Vec::new();
    let mut narrowed = false;
    let (mut len, mut members) = (0usize, 0usize);
    for (i, p) in partitions.iter().enumerate() {
        let full = len == ceiling;
        if starts.is_empty() || full || members.saturating_add(p.members) > member_cap {
            narrowed |= !starts.is_empty() && !full;
            starts.push(i);
            (len, members) = (0, 0);
        }
        len += 1;
        members = members.saturating_add(p.members);
    }
    (starts, narrowed)
}

// ============================================================================
// Pass-2 single-read fan-out for partitioning mode (issue #235)
// ============================================================================
//
// In partitioning mode a level's render set is the accumulating row-group
// prefix `0..=end_k` (spec §5.1), so the legacy pass 2 — one bbox-pruned
// prefix read per wave per level — re-reads and re-decodes a coarse row group
// once per finer level per wave. The machinery below reads each band exactly
// once (the pass-2 analogue of #233's scan fix), clips every feature at every
// including level's zoom during that single pass, and buffers the clipped
// members in a per-(level, wave) [`MemberStore`] until the drain loop encodes
// them in the historical level/partition/tile order.
//
// **Byte identity.** A tile member's clipped geometry is key-window
// independent (pinned by `recursive_split_is_partition_range_invariant`), so
// clipping once over the full key range and routing by key yields the same
// members the per-wave range-restricted clips produced. Within-tile order is
// carried by `seq` — here the global row index in band order, which restricts
// to exactly the relative order the legacy pruned prefix reads yielded — and
// `encode_members` sorts by the unique `(key, seq)` before grouping, so buffer
// and spill order are irrelevant. The drain replays the identical ascending
// (level, partition, tile) write order.
//
// **Memory.** The store's RAM footprint follows the converter's pass-2 sink
// policy for the run's profile ([`fill_backing`], #788): `speed` RAM,
// `bounded` spill, `auto` keeps small buffered sets in RAM and spills large
// ones' member records to one temp file. Under spill backing the in-RAM buckets are
// bounded by [`MEMBER_STORE_RAM_BUDGET`] during the fill (largest buckets are
// flushed first, in whole segments, so spill I/O stays chunky and sequential),
// and every bucket is flushed at fill end. The drain then holds only one
// wave's members at a time — the same `O(one wave of partitions)` ceiling as
// the legacy path.

/// In-RAM ceiling (estimated bytes) for the member store's not-yet-flushed
/// buckets under spill backing. Crossing it flushes the largest buckets down
/// to half the ceiling; the fill's buffered transient is therefore bounded
/// regardless of input size, while flushes stay large enough to keep the spill
/// file's segments chunky.
pub(super) const MEMBER_STORE_RAM_BUDGET: usize = 128 * 1024 * 1024;

/// Whole spill segments per bucket the member store's fill buffer holds
/// once its members are measured: two, so flushing the largest bucket down
/// to half the buffer writes at least about one full segment.
pub(super) const STORE_BUFFER_SEGMENTS: usize = 2;

/// How a [`MemberStore`] holds its members: in RAM, or spilled with a fill
/// buffer of at most `ram_budget` bytes of not-yet-flushed buckets, plus up
/// to `resident` bytes that stay in RAM after the fill instead of spilling.
#[derive(Debug, Clone, Copy)]
pub(super) struct StoreBacking {
    pub(super) backing: SinkBacking,
    pub(super) ram_budget: usize,
    pub(super) resident: usize,
}

/// How a level of several waves reads its band when no export-wide member
/// store is filled (#740): once, into a one-level [`MemberStore`].
#[derive(Debug, Clone, Copy)]
pub(super) struct LevelRead {
    /// A backing the caller forced (tests), or `None` for the budget's
    /// choice ([`level_read_backing`]).
    pub(super) forced: Option<SinkBacking>,
    /// The fill buffer of a spilling store: the run's store grant.
    pub(super) ram_budget: usize,
    /// Resident bytes of one member (a footer price, see
    /// [`member_resident_bytes`]), when known.
    pub(super) member_bytes: Option<u64>,
    /// The run's memory profile: `bounded` grants the wave only and
    /// spills the level, `speed` keeps the whole level in RAM, `auto` keeps
    /// what the budget affords.
    pub(super) profile: MemoryProfile,
}

/// Choose a multi-wave level's store backing under `budget` (#740): RAM
/// when the level's planned members fit [`MemoryPhase::ExportCollect`]
/// after the drain's widest wave, otherwise spill, keeping as many bytes
/// resident as the grant covers and spilling only the remainder. Members
/// are priced at the footer's per-member bytes. Spills whole when RAM or
/// the member size is unknown. Returns the grants the level holds until
/// its last wave is written.
pub(super) fn level_read_backing(
    read: LevelRead,
    plan: &LevelPlan,
    budget: &mut MemoryBudget,
    wave_floor: usize,
) -> (StoreBacking, Vec<Grant>) {
    let spill = StoreBacking {
        backing: SinkBacking::Spill,
        ram_budget: read.ram_budget,
        resident: 0,
    };
    if let Some(backing) = read.forced {
        return (StoreBacking { backing, ..spill }, Vec::new());
    }
    let (Some(per_member), Some(_)) = (read.member_bytes.filter(|&b| b > 0), budget.limit) else {
        return (spill, Vec::new());
    };
    let price = Price::new(per_member, PriceBasis::Footer);
    let members = |ps: &[Partition]| ps.iter().map(|p| p.members).sum::<usize>();
    let level_members = members(&plan.partitions);
    let wave_members = plan.waves().map(members).max().unwrap_or(0).max(wave_floor);
    // The wave the drain holds while it encodes, which the wave width was
    // sized to fit; the level's members share what is left.
    let wave = budget.fit(
        read.profile,
        MemoryPhase::ExportCollect,
        price,
        wave_members,
        wave_members,
    );
    let affordable = budget.affordable(read.profile, MemoryPhase::ExportCollect, price);
    let level = budget.fit(
        read.profile,
        MemoryPhase::ExportCollect,
        price,
        level_members,
        0,
    );
    let affordable = if affordable == usize::MAX {
        "unlimited".to_string()
    } else {
        format!(
            "{} MiB",
            (affordable as u64).saturating_mul(per_member) >> 20
        )
    };
    log::debug!(target: LOG_TARGET,
        "[profile]     z{} level store grants: wave {} MiB ({wave_members} members), \
         level {} of {} MiB wanted ({level_members} members at {per_member} B), \
         {affordable} was affordable",
        plan.zoom,
        wave.bytes() >> 20,
        level.bytes() >> 20,
        (level_members as u64).saturating_mul(per_member) >> 20,
    );
    let backing = if level.units >= level_members {
        StoreBacking {
            backing: SinkBacking::Ram,
            ..spill
        }
    } else {
        let resident = usize::try_from(level.bytes()).unwrap_or(usize::MAX);
        StoreBacking { resident, ..spill }
    };
    (backing, vec![wave, level])
}

/// Bounded-channel depth between the band reader thread and the clip/route
/// consumer of the single-read fill — the read/compute overlap and
/// backpressure knob, mirroring `overview/pipeline.rs`.
pub(super) const SINGLE_READ_IN_FLIGHT: usize = 4;

/// Bounded-channel depth between the reader thread and the consumer of one
/// **duplicating**-mode wave ([`process_wave`], #535) and of the fan-out bbox
/// scan ([`scan_all_levels`]).
///
/// Deliberately shallower than [`SINGLE_READ_IN_FLIGHT`]. The wave path also
/// holds a whole wave's members in RAM, and the scan runs before any wave
/// preflight, so in-flight batches on either land outside what the wave-width
/// preflight ([`memory_safe_wave_members`]) budgets. Two is all the overlap
/// needs — one batch being consumed while the next is read.
///
/// The live Arrow batches per pipe are bounded by **depth + 2**, not depth:
/// `depth` queued in the channel, one the producer has finished decoding and
/// is blocked sending, and one the consumer is working on — so up to four
/// `EXPORT_BATCH_SIZE` batches here. For a wave that is noise next to its own
/// member buffer and is not folded into the preflight estimate; the scan
/// additionally projects its read to the geometry column
/// ([`read_in_order_capped`] with a projection), so its batches carry no
/// property columns.
pub(super) const WAVE_READ_IN_FLIGHT: usize = 2;

pub(super) fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub(super) fn put_u64(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// Coordinates spill as raw IEEE-754 bit patterns (`to_bits`/`from_bits`), so
/// the round-trip is bit-exact — including negative zero — and the re-encoded
/// MVT bytes cannot drift through the spill.
pub(super) fn put_coord(buf: &mut Vec<u8>, c: geo::Coord<f64>) {
    put_u64(buf, c.x.to_bits());
    put_u64(buf, c.y.to_bits());
}

pub(super) fn put_line_string(buf: &mut Vec<u8>, ls: &geo::LineString<f64>) {
    put_u32(buf, ls.0.len() as u32);
    for c in &ls.0 {
        put_coord(buf, *c);
    }
}

pub(super) fn put_polygon(buf: &mut Vec<u8>, p: &geo::Polygon<f64>) {
    put_u32(buf, 1 + p.interiors().len() as u32);
    put_line_string(buf, p.exterior());
    for r in p.interiors() {
        put_line_string(buf, r);
    }
}

/// Serialize one geometry for the member spill (tag byte + payload). Covers
/// every [`Geometry`] variant so clip pass-throughs of any decoded input shape
/// survive the spill.
pub(super) fn encode_geometry(buf: &mut Vec<u8>, g: &Geometry<f64>) {
    match g {
        Geometry::Point(p) => {
            buf.push(0);
            put_coord(buf, p.0);
        }
        Geometry::Line(l) => {
            buf.push(1);
            put_coord(buf, l.start);
            put_coord(buf, l.end);
        }
        Geometry::LineString(ls) => {
            buf.push(2);
            put_line_string(buf, ls);
        }
        Geometry::Polygon(p) => {
            buf.push(3);
            put_polygon(buf, p);
        }
        Geometry::MultiPoint(mp) => {
            buf.push(4);
            put_u32(buf, mp.0.len() as u32);
            for p in &mp.0 {
                put_coord(buf, p.0);
            }
        }
        Geometry::MultiLineString(mls) => {
            buf.push(5);
            put_u32(buf, mls.0.len() as u32);
            for ls in &mls.0 {
                put_line_string(buf, ls);
            }
        }
        Geometry::MultiPolygon(mp) => {
            buf.push(6);
            put_u32(buf, mp.0.len() as u32);
            for p in &mp.0 {
                put_polygon(buf, p);
            }
        }
        Geometry::GeometryCollection(gc) => {
            buf.push(7);
            put_u32(buf, gc.0.len() as u32);
            for g in &gc.0 {
                encode_geometry(buf, g);
            }
        }
        Geometry::Rect(r) => {
            buf.push(8);
            put_coord(buf, r.min());
            put_coord(buf, r.max());
        }
        Geometry::Triangle(t) => {
            buf.push(9);
            put_coord(buf, t.v1());
            put_coord(buf, t.v2());
            put_coord(buf, t.v3());
        }
    }
}

/// Serialize one member: key, seq, gap, band, feature id, properties,
/// geometry.
/// Property floats spill as bit patterns for the same exactness guarantee as
/// coordinates.
pub(super) fn encode_member(buf: &mut Vec<u8>, m: &Member, names: &SpillNames) {
    put_u64(buf, m.key);
    put_u64(buf, m.seq);
    put_u64(buf, m.rank);
    put_u64(buf, m.gap);
    put_u32(buf, u32::from(m.band));
    match m.id {
        Some(id) => {
            buf.push(1);
            put_u64(buf, id);
        }
        None => buf.push(0),
    }
    put_u32(buf, names.id(m.props.names()));
    // `packed_props::arrow::extract` rejects a row the u32 cannot frame.
    put_u32(
        buf,
        u32::try_from(m.props.bytes().len()).expect("row length checked at extraction"),
    );
    buf.extend_from_slice(m.props.bytes());
    encode_geometry(buf, &m.geom);
}

/// The "spill decode failed" error: the temp file is written and read by this
/// process only, so any mismatch is a bug (or disk corruption), never user
/// input.
pub(super) fn spill_corrupt() -> ExportError {
    ExportError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "member spill decode: truncated or corrupt record",
    ))
}

/// Bounds-checked reader over one spill segment's bytes.
pub(super) struct SpillCursor<'a> {
    pub(super) buf: &'a [u8],
    pub(super) pos: usize,
}

impl<'a> SpillCursor<'a> {
    pub(super) fn new(buf: &'a [u8]) -> Self {
        SpillCursor { buf, pos: 0 }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    pub(super) fn take(&mut self, n: usize) -> Result<&'a [u8], ExportError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.buf.len());
        let Some(end) = end else {
            return Err(spill_corrupt());
        };
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// `count` items of at least `min_item` bytes each, as an allocation
    /// size: never more than the bytes left could hold, so a corrupt count
    /// cannot ask for more memory than the segment is long.
    pub(super) fn bounded(&self, count: u64, min_item: usize) -> usize {
        let room = (self.buf.len() - self.pos) / min_item;
        usize::try_from(count).map_or(room, |c| c.min(room))
    }

    pub(super) fn u8(&mut self) -> Result<u8, ExportError> {
        Ok(self.take(1)?[0])
    }

    pub(super) fn u32(&mut self) -> Result<u32, ExportError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    pub(super) fn u64(&mut self) -> Result<u64, ExportError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    pub(super) fn coord(&mut self) -> Result<geo::Coord<f64>, ExportError> {
        let x = f64::from_bits(self.u64()?);
        let y = f64::from_bits(self.u64()?);
        Ok(geo::coord! { x: x, y: y })
    }

    pub(super) fn line_string(&mut self) -> Result<geo::LineString<f64>, ExportError> {
        let n = self.u32()? as usize;
        let mut coords = Vec::with_capacity(self.bounded(n as u64, 16));
        for _ in 0..n {
            coords.push(self.coord()?);
        }
        Ok(geo::LineString(coords))
    }

    pub(super) fn polygon(&mut self) -> Result<geo::Polygon<f64>, ExportError> {
        let rings = self.u32()? as usize;
        if rings == 0 {
            return Err(spill_corrupt());
        }
        let exterior = self.line_string()?;
        let mut interiors = Vec::with_capacity(self.bounded(rings as u64 - 1, 4));
        for _ in 1..rings {
            interiors.push(self.line_string()?);
        }
        Ok(geo::Polygon::new(exterior, interiors))
    }
}

pub(super) fn decode_geometry(cur: &mut SpillCursor<'_>) -> Result<Geometry<f64>, ExportError> {
    Ok(match cur.u8()? {
        0 => Geometry::Point(geo::Point(cur.coord()?)),
        1 => Geometry::Line(geo::Line::new(cur.coord()?, cur.coord()?)),
        2 => Geometry::LineString(cur.line_string()?),
        3 => Geometry::Polygon(cur.polygon()?),
        4 => {
            let n = cur.u32()? as usize;
            let mut pts = Vec::with_capacity(cur.bounded(n as u64, 16));
            for _ in 0..n {
                pts.push(geo::Point(cur.coord()?));
            }
            Geometry::MultiPoint(geo::MultiPoint(pts))
        }
        5 => {
            let n = cur.u32()? as usize;
            let mut lines = Vec::with_capacity(cur.bounded(n as u64, 4));
            for _ in 0..n {
                lines.push(cur.line_string()?);
            }
            Geometry::MultiLineString(geo::MultiLineString(lines))
        }
        6 => {
            let n = cur.u32()? as usize;
            let mut polys = Vec::with_capacity(cur.bounded(n as u64, 8));
            for _ in 0..n {
                polys.push(cur.polygon()?);
            }
            Geometry::MultiPolygon(geo::MultiPolygon(polys))
        }
        7 => {
            let n = cur.u32()? as usize;
            let mut geoms = Vec::with_capacity(cur.bounded(n as u64, 1));
            for _ in 0..n {
                geoms.push(decode_geometry(cur)?);
            }
            Geometry::GeometryCollection(geo::GeometryCollection(geoms))
        }
        8 => Geometry::Rect(geo::Rect::new(cur.coord()?, cur.coord()?)),
        9 => Geometry::Triangle(geo::Triangle::new(cur.coord()?, cur.coord()?, cur.coord()?)),
        _ => return Err(spill_corrupt()),
    })
}

/// Schema tables retained beside the process-private spill. No names or
/// values are reconstructed per member when a segment is reloaded.
#[derive(Default)]
pub(super) struct SpillNames {
    pub(super) tables: Vec<PropertyNames>,
}

impl SpillNames {
    /// Keeps the caller's own handle, so [`Self::id`] hits on pointer
    /// identity before it falls back to comparing names.
    pub(super) fn register(&mut self, table: &PropertyNames) {
        if !self
            .tables
            .iter()
            .any(|t| Arc::ptr_eq(t, table) || t == table)
        {
            self.tables.push(table.clone());
        }
    }

    pub(super) fn id(&self, table: &PropertyNames) -> u32 {
        self.tables
            .iter()
            .position(|t| Arc::ptr_eq(t, table) || t == table)
            .expect("registered spill schema") as u32
    }
}

/// Smallest encoded member: its fixed header, before any property or geometry
/// byte (key, seq, gap, band, id flag, schema id, row length).
pub(super) const MEMBER_WIRE_MIN: usize = 37;

pub(super) fn decode_segment(buf: &[u8], names: &SpillNames) -> Result<Vec<Member>, ExportError> {
    let mut cur = SpillCursor::new(buf);
    let n = cur.u64()?;
    // A member is never smaller than its fixed header, so a count the buffer
    // cannot hold is corrupt; never trust it for an allocation.
    let mut members = Vec::with_capacity(cur.bounded(n, MEMBER_WIRE_MIN));
    // PERF (#803): one copy of each table per segment, so the pool threads
    // decoding segments do not share one table's refcount.
    let local = SpillNames {
        tables: names
            .tables
            .iter()
            .map(|t| t.iter().cloned().collect::<Vec<_>>().into())
            .collect(),
    };
    for _ in 0..n {
        members.push(decode_member(&mut cur, &local)?);
    }
    if !cur.is_empty() {
        return Err(spill_corrupt());
    }
    Ok(members)
}

pub(super) fn decode_member(
    cur: &mut SpillCursor<'_>,
    names: &SpillNames,
) -> Result<Member, ExportError> {
    let key = cur.u64()?;
    let seq = cur.u64()?;
    let rank = cur.u64()?;
    let gap = cur.u64()?;
    let band = u16::try_from(cur.u32()?).map_err(|_| spill_corrupt())?;
    let id = match cur.u8()? {
        0 => None,
        1 => Some(cur.u64()?),
        _ => return Err(spill_corrupt()),
    };
    let schema = cur.u32()? as usize;
    let names = names.tables.get(schema).ok_or_else(spill_corrupt)?.clone();
    let len = cur.u32()? as usize;
    let props = SharedProps::from_spill(names, Arc::from(cur.take(len)?))?;
    let geom = decode_geometry(cur)?;
    Ok(Member {
        key,
        seq,
        rank,
        geom: geom.into(),
        props,
        id,
        gap,
        band,
    })
}

/// Rough in-RAM cost of one buffered member, for the fill-phase budget only
/// (never output-bound): the struct, 16 B per coordinate, and the packed
/// property row (spilled per member, so counted per member). The row's
/// schema table is shared and not charged (#461).
pub(super) fn member_bytes_estimate(m: &Member) -> usize {
    std::mem::size_of::<Member>() + m.geom.coords_count() * 16 + m.props.bytes().len()
}

/// The single spill file behind a [`MemberStore`]: bucket flushes append
/// length-indexed segments; `segments[level][wave]` records each segment in
/// append order.
pub(super) struct MemberSpill {
    pub(super) names: SpillNames,
    pub(super) writer: BufWriter<File>,
    pub(super) read: File,
    /// Keeps the anonymous temp file alive (unlinked on drop).
    _temp: NamedTempFile,
    pub(super) offset: u64,
    pub(super) segments: Vec<Vec<Vec<SpillSegment>>>,
}

/// One spill segment: `members` records at `(offset, len)` in the file.
#[derive(Clone, Copy)]
pub(super) struct SpillSegment {
    pub(super) offset: u64,
    pub(super) len: u64,
    pub(super) members: usize,
}

/// Members per spill segment. A flush encodes, and `take_wave` decodes, its
/// segments in parallel (#740), so a bucket is cut into many: under spill a
/// multi-wave level's whole band passes through here, and serially the
/// encode and decode were 37 core-seconds on the consumer thread of an NZ
/// parcels export.
pub(super) const SPILL_SEGMENT_MEMBERS: usize = 4096;

/// Raw segment bytes [`MemberStore::take_wave`] reads ahead of its parallel
/// decode: a bound on the read buffer next to the wave's decoded members.
pub(super) const SPILL_READ_GROUP_BYTES: u64 = 32 * 1024 * 1024;

/// Per-(level, wave) buffered tile members for the partitioning single-read
/// pass 2 (#235), with RAM or spill backing. See the section comment above for
/// the ordering and memory arguments.
pub(super) struct MemberStore {
    /// `[level][wave]` in-RAM member buckets.
    pub(super) buckets: Vec<Vec<Vec<Member>>>,
    /// Estimated bytes held by each in-RAM bucket.
    pub(super) bucket_bytes: Vec<Vec<usize>>,
    /// Total estimated bytes across all in-RAM buckets.
    pub(super) buffered_bytes: usize,
    /// Present under spill backing only.
    pub(super) spill: Option<MemberSpill>,
    /// The in-RAM ceiling for not-yet-flushed buckets under spill backing.
    /// It starts at the run's [`MemoryPhase::ExportCollect`] grant (a model
    /// price, [`MEMBER_STORE_RAM_BUDGET`]) and, once the first
    /// [`SPILL_SEGMENT_MEMBERS`] members are measured, narrows to
    /// [`STORE_BUFFER_SEGMENTS`] segments per bucket at their measured mean
    /// size: enough for every flush to write whole segments.
    pub(super) buffer: usize,
    /// Bytes of buckets the fill leaves in RAM instead of spilling them
    /// (#740): a multi-wave level's grant beyond the fill buffer. Zero for
    /// the export-wide store, whose drain holds one wave at a time.
    pub(super) resident: usize,
    /// Members pushed so far and their estimated bytes: the measurement
    /// [`Self::buffer`] is priced from.
    pub(super) pushed_members: usize,
    pub(super) pushed_bytes: usize,
    /// `[level][wave]` buckets in all.
    pub(super) bucket_count: usize,
    /// Profiling only (#535): cumulative time spent in [`Self::flush_bucket`]
    /// (serialize + write of spill segments). Read by the fill to charge it
    /// to `spill_write` rather than to the routing that triggered it.
    pub(super) flush_time: Duration,
}

impl MemberStore {
    /// `spill_dir` places the spill file (#427); `None` is the process temp
    /// dir.
    pub(super) fn new(
        plans: &[LevelPlan],
        backing: SinkBacking,
        ram_budget: usize,
        spill_dir: Option<&Path>,
    ) -> Result<Self, ExportError> {
        let shape: Vec<usize> = plans.iter().map(LevelPlan::wave_count).collect();
        let buckets = shape
            .iter()
            .map(|&w| (0..w).map(|_| Vec::new()).collect())
            .collect();
        let bucket_bytes = shape.iter().map(|&w| vec![0usize; w]).collect();
        let bucket_count = shape.iter().sum();
        let spill = match backing {
            SinkBacking::Ram => None,
            SinkBacking::Spill => {
                let temp = super::super::pipeline::spill_temp_file(spill_dir)?;
                let writer = BufWriter::new(temp.reopen()?);
                let read = temp.reopen()?;
                Some(MemberSpill {
                    names: SpillNames::default(),
                    writer,
                    read,
                    _temp: temp,
                    offset: 0,
                    segments: shape
                        .iter()
                        .map(|&w| (0..w).map(|_| Vec::new()).collect())
                        .collect(),
                })
            }
        };
        Ok(MemberStore {
            buckets,
            bucket_bytes,
            buffered_bytes: 0,
            spill,
            buffer: ram_budget,
            resident: 0,
            pushed_members: 0,
            pushed_bytes: 0,
            bucket_count,
            flush_time: Duration::ZERO,
        })
    }

    /// The fill's in-RAM ceiling: the buffer plus the resident grant.
    pub(super) fn ram_budget(&self) -> usize {
        self.buffer.saturating_add(self.resident)
    }

    /// Bytes written to the spill file so far.
    pub(super) fn spilled_bytes(&self) -> u64 {
        self.spill.as_ref().map_or(0, |s| s.offset)
    }

    /// #810: bytes of `level`'s members still on disk (all of them before
    /// its first wave is taken).
    pub(super) fn level_spilled_bytes(&self, level: usize) -> u64 {
        self.spill.as_ref().map_or(0, |s| {
            s.segments[level].iter().flatten().map(|seg| seg.len).sum()
        })
    }

    pub(super) fn push(&mut self, level: usize, wave: usize, m: Member) -> Result<(), ExportError> {
        let est = member_bytes_estimate(&m);
        self.buckets[level][wave].push(m);
        self.bucket_bytes[level][wave] += est;
        self.buffered_bytes += est;
        self.pushed_members += 1;
        self.pushed_bytes += est;
        if self.pushed_members == SPILL_SEGMENT_MEMBERS {
            // A measured price: the members this store already holds.
            let per_member = self.pushed_bytes / self.pushed_members;
            let need = self
                .bucket_count
                .saturating_mul(STORE_BUFFER_SEGMENTS * SPILL_SEGMENT_MEMBERS)
                .saturating_mul(per_member);
            self.buffer = self.buffer.min(need);
        }
        if self.spill.is_some() && self.buffered_bytes > self.ram_budget() {
            // Flush half of the fill buffer at a time, so a store with a
            // resident grant spills only what does not fit.
            self.flush_largest_until(self.ram_budget() - self.buffer / 2)?;
        }
        Ok(())
    }

    /// Flush the largest in-RAM buckets (whole segments — chunky, sequential
    /// spill writes) until the buffered estimate drops to `floor`.
    pub(super) fn flush_largest_until(&mut self, floor: usize) -> Result<(), ExportError> {
        while self.buffered_bytes > floor {
            let mut best = (0usize, 0usize, 0usize);
            for (li, level) in self.bucket_bytes.iter().enumerate() {
                for (wi, &b) in level.iter().enumerate() {
                    if b > best.2 {
                        best = (li, wi, b);
                    }
                }
            }
            if best.2 == 0 {
                break;
            }
            self.flush_bucket(best.0, best.1)?;
        }
        Ok(())
    }

    /// Serialize one bucket's members as a single appended spill segment and
    /// clear the bucket.
    pub(super) fn flush_bucket(&mut self, level: usize, wave: usize) -> Result<(), ExportError> {
        use std::io::Write;
        let members = std::mem::take(&mut self.buckets[level][wave]);
        let bytes = std::mem::take(&mut self.bucket_bytes[level][wave]);
        self.buffered_bytes -= bytes;
        if members.is_empty() {
            return Ok(());
        }
        let t_flush = Instant::now();
        let spill = self
            .spill
            .as_mut()
            .expect("flush_bucket requires spill backing");
        for m in &members {
            spill.names.register(m.props.names());
        }
        // A few segments per pool thread at a time, at most 16, so the
        // encoded bytes held before their write stay a small, bounded
        // transient next to the bucket (dhat: encoding a whole bucket into
        // one buffer was half of a Moldova export's peak live heap).
        let group = SPILL_SEGMENT_MEMBERS * rayon::current_num_threads().clamp(1, 16);
        for members in members.chunks(group) {
            let encoded: Vec<(Vec<u8>, usize)> = members
                .par_chunks(SPILL_SEGMENT_MEMBERS)
                .map(|chunk| {
                    let mut buf = Vec::new();
                    put_u64(&mut buf, chunk.len() as u64);
                    for m in chunk {
                        encode_member(&mut buf, m, &spill.names);
                    }
                    (buf, chunk.len())
                })
                .collect();
            for (buf, n) in encoded {
                spill.writer.write_all(&buf)?;
                spill.segments[level][wave].push(SpillSegment {
                    offset: spill.offset,
                    len: buf.len() as u64,
                    members: n,
                });
                spill.offset += buf.len() as u64;
            }
        }
        members.into_par_iter().for_each(drop);
        self.flush_time += t_flush.elapsed();
        Ok(())
    }

    /// Finish the fill phase. Under spill backing the buckets above the
    /// store's resident grant are flushed, the largest first, and with no
    /// grant every bucket is (the store was chosen *because* the buffered
    /// set is large; the tail is negligible and this keeps the drain's RAM
    /// at one wave). The writer is then flushed so `take_wave` reads observe
    /// every segment.
    pub(super) fn finish_fill(&mut self) -> Result<(), ExportError> {
        use std::io::Write;
        if self.spill.is_some() {
            if self.resident > 0 {
                self.flush_largest_until(self.resident)?;
            } else {
                for li in 0..self.buckets.len() {
                    for wi in 0..self.buckets[li].len() {
                        self.flush_bucket(li, wi)?;
                    }
                }
            }
            let t_flush = Instant::now();
            self.spill
                .as_mut()
                .expect("spill checked above")
                .writer
                .flush()?;
            self.flush_time += t_flush.elapsed();
        }
        Ok(())
    }

    /// Take one wave's members out of the store: spill segments (in append
    /// order) followed by any in-RAM remainder. Member order is irrelevant to
    /// the output — `encode_members` sorts by the unique `(key, seq)`.
    ///
    /// Segments are read in groups of about [`SPILL_READ_GROUP_BYTES`] and
    /// each group's segments decoded in parallel (#740). Each segment copies the
    /// schema tables retained beside the spill once, and its members share that copy.
    pub(super) fn take_wave(
        &mut self,
        level: usize,
        wave: usize,
    ) -> Result<Vec<Member>, ExportError> {
        use std::io::{Read, Seek, SeekFrom};
        let mut out = Vec::new();
        if let Some(spill) = self.spill.as_mut() {
            let segments = std::mem::take(&mut spill.segments[level][wave]);
            if !segments.is_empty() {
                out.reserve_exact(
                    segments.iter().map(|s| s.members).sum::<usize>()
                        + self.buckets[level][wave].len(),
                );
            }
            let mut rest = segments.as_slice();
            while !rest.is_empty() {
                let mut group_bytes = 0u64;
                let take = rest
                    .iter()
                    .take_while(|s| {
                        let first = group_bytes == 0;
                        group_bytes += s.len;
                        first || group_bytes <= SPILL_READ_GROUP_BYTES
                    })
                    .count();
                let (group, tail) = rest.split_at(take);
                rest = tail;
                let mut bufs = Vec::with_capacity(group.len());
                for seg in group {
                    spill.read.seek(SeekFrom::Start(seg.offset))?;
                    let mut buf = vec![0u8; seg.len as usize];
                    spill.read.read_exact(&mut buf)?;
                    bufs.push(buf);
                }
                let decoded: Vec<Vec<Member>> = bufs
                    .into_par_iter()
                    .map(|buf| decode_segment(&buf, &spill.names))
                    .collect::<Result<_, _>>()?;
                for mut members in decoded {
                    out.append(&mut members);
                }
            }
        }
        let bytes = std::mem::take(&mut self.bucket_bytes[level][wave]);
        self.buffered_bytes -= bytes;
        // Move a RAM wave's allocation into the drain. `append` on the
        // stored bucket would retain it while the drain builds its buckets.
        let mut members = std::mem::take(&mut self.buckets[level][wave]);
        if out.is_empty() {
            return Ok(members);
        }
        out.append(&mut members);
        Ok(out)
    }
}

/// The single-read fill (#235): a producer thread streams every band once,
/// in band order, over a bounded channel (mirroring `overview/pipeline.rs`),
/// each band through the ordered parallel reader ([`send_bands`], #833); the
/// consumer decodes each batch once and fans its clipped members to every
/// level whose render set includes the band — in partitioning mode, band `j`
/// feeds levels `{j..N}`.
pub(super) fn fill_member_store(
    reader: &OverviewReader,
    ctx: &FanoutCtx<'_>,
    backing: StoreBacking,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<MemberStore, ExportError> {
    let timers = ctx.timers;
    let plans = ctx.plans;
    let num_levels = plans.len();
    let mut store = MemberStore::new(
        plans,
        backing.backing,
        backing.ram_budget,
        ctx.opts.spill_dir.as_deref(),
    )?;
    let t_fill = Instant::now();
    let mut seq = 0u64;
    // #745: every band is read whole and in order, so the rows arrive in
    // file order across every row group.
    let all_row_groups: Vec<usize> = (0..reader.num_row_groups()).collect();
    let mut gaps = RowGapCursor::new(reader.row_group_bands(&all_row_groups))
        .measuring(valve_merges(ctx.opts));
    let store_ref = &mut store;
    let seq_ref = &mut seq;
    let gaps_ref = &mut gaps;
    #[cfg(test)]
    let mut readers = 0;
    #[cfg(test)]
    let readers_ref = &mut readers;
    scoped_pipe(
        SINGLE_READ_IN_FLIGHT,
        |tx: &Sender<(usize, RecordBatch)>| -> Result<(), ExportError> {
            // #535: `band_read` is the readers' time, not the `tx.send`
            // (a bounded-channel send that can block on the consumer).
            send_bands(
                (reader, ctx.band),
                None,
                ReadPass::Build,
                |read| add_nanos(&timers.band_read, read),
                tx,
                #[cfg(test)]
                readers_ref,
            )
        },
        // Consumer: batches arrive in band order (bands are contiguous
        // ascending row groups), so the running `seq` is the global file row
        // index — the band-order sequence every level's within-tile ordering
        // derives from.
        |rx: Receiver<(usize, RecordBatch)>| -> Result<(), ExportError> {
            for (band, batch) in rx.iter() {
                fanout_batch_members(&batch, band, ctx, seq_ref, gaps_ref, store_ref, diagnostics)?;
            }
            Ok(())
        },
    )?;
    #[cfg(test)]
    BAND_READ_WORKERS.with(|w| w.set(readers));
    store.finish_fill()?;
    // Every spill flush (during routing and the final one) → `spill_write`;
    // `fanout_batch_members` already kept its share out of `decode`.
    add_nanos(&timers.spill_write, store.flush_time);
    log::info!(target: LOG_TARGET,
        "[export] pass2 fill (#235): {seq} row(s) read once and fanned across \
         {num_levels} level(s) in {:.2}s",
        t_fill.elapsed().as_secs_f64(),
    );
    Ok(store)
}

/// Decode one band batch and fan its members out: per including level, clip
/// every feature (in parallel) at the level's zoom over the **full** key
/// range, then route each member to its `(level, partition wave)` bucket
/// before the next level is clipped, so one level's clipped batch is held at
/// a time. A member the clip left whole shares the feature's decoded
/// geometry with every other level's (#815). The legacy analogue is
/// [`finish_group`]; see the section comment for why the results are
/// byte-identical.
pub(super) fn fanout_batch_members(
    batch: &RecordBatch,
    band: usize,
    ctx: &FanoutCtx<'_>,
    seq: &mut u64,
    gaps: &mut RowGapCursor,
    store: &mut MemberStore,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ExportError> {
    let FanoutCtx {
        crs,
        plans,
        opts,
        published,
        feature_id,
        timers,
        ..
    } = *ctx;
    let schema = batch.schema();
    let geom_idx = batch_geometry_index(batch, published.geometry.as_deref())?;
    let geoms = decode_batch_geometries(batch, &schema, geom_idx, crs, timers, diagnostics)?;
    let row_gaps = ExportTimers::time(&timers.decode, || gaps.batch(&geoms));
    debug_assert!(row_gaps.iter().all(|&(_, b)| usize::from(b) == band));
    let geoms: Vec<Arc<Geometry<f64>>> = geoms.into_iter().map(Arc::new).collect();

    // Band `j` belongs to every level `k >= j`'s render set (partitioning
    // prefix). A level with no planned partitions sized no tiles, so no
    // feature in its render set can emit members — skip it (routing into an
    // empty plan would be unreachable anyway).
    let targets: Vec<usize> = (band..plans.len())
        .filter(|&k| !plans[k].partitions.is_empty())
        .collect();

    // #535: property extraction and routing into the store are `decode`,
    // minus any spill flush a `store.push` triggers, which the fill charges
    // to `spill_write` instead.
    let t_route = Instant::now();
    let flushed_before = store.flush_time;
    // Each row's properties, materialized once and shared via `Arc` by its
    // members at every level (as the legacy path does per wave).
    let cols = AttrColumns::new(&schema, geom_idx, published, feature_id);
    let mut attrs = cols.extract(batch)?;
    let ids: Vec<Option<u64>> = (0..batch.num_rows())
        .map(|row| attrs.id(row))
        .collect::<Result<_, _>>()?;
    // Built at a row's first member, so a memberless row allocates none.
    let mut row_props: Vec<Option<SharedProps>> = vec![None; batch.num_rows()];
    for (row, id) in ids.iter().enumerate() {
        if let Some(id) = *id {
            for &k in &targets {
                diagnostics.source_member_loss(
                    *seq + row as u64,
                    id,
                    plans[k].zoom,
                    crate::diagnostics::SourceLossCause::ClipDropped,
                )?;
            }
        }
    }
    let mut routed = t_route.elapsed();
    for &k in &targets {
        let plan = &plans[k];
        let finest = (k + 1 == plans.len()).then_some((plan.zoom, ctx.pins));
        let clipped: RowMembers = geoms
            .par_iter()
            .map(|g| {
                // #876: a clip a tile's thread runs is foreign to the tile.
                crate::tile_trace::job(
                    || crate::tile_trace::Ctx::zoom(plan.zoom),
                    || {
                        ExportTimers::time(&timers.clip, || {
                            shared_tile_members(
                                MemberSource::Shared(g),
                                finest,
                                plan.zoom,
                                opts,
                                (0, u64::MAX),
                                diagnostics,
                            )
                        })
                    },
                )
            })
            .collect();
        let t_level = Instant::now();
        for (row, items) in clipped.into_iter().enumerate() {
            if items.is_empty() {
                continue;
            }
            let props = row_props[row].get_or_insert_with(|| attrs.props(row));
            let id = ids[row];
            let (gap, band) = row_gaps[row];
            for (key, geom) in items {
                let pidx = route_partition(&plan.partitions, key);
                store.push(
                    k,
                    plan.wave_of(pidx),
                    Member {
                        key,
                        seq: *seq + row as u64,
                        rank: attrs.rank(row, *seq + row as u64),
                        geom,
                        props: props.clone(),
                        id,
                        gap,
                        band,
                    },
                )?;
            }
        }
        routed += t_level.elapsed();
    }
    *seq += batch.num_rows() as u64;
    let flushed = store.flush_time.saturating_sub(flushed_before);
    add_nanos(&timers.decode, routed.saturating_sub(flushed));
    Ok(())
}

/// Per row of a batch, the row's `(key, clipped geometry)` members at one
/// target level.
pub(super) type RowMembers = Vec<TileMembers>;

/// A feature's MVT properties, shared across its tile members.
///
/// One byte buffer per row, with names shared through a schema table.
/// The encoder borrows strings directly from this buffer.
pub(super) type PropertyNames = Arc<[Arc<str>]>;
pub(super) type SharedProps = packed_props::PackedProps;

/// Pass-2 drain for one wave under the single-read path: take the wave's
/// buffered members, bucket them per partition (exactly as
/// [`finish_group`] routes), and encode each partition as
/// [`process_wave`] would — same parallel section, same ascending output
/// order.
pub(super) fn encode_wave_from_store(
    store: &mut MemberStore,
    store_level: usize,
    ctx: &LevelCtx<'_>,
    wave_idx: usize,
    wave: &[Partition],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<Vec<EncodedTile>>, ExportError> {
    let members = ExportTimers::time(&ctx.timers.spill_read, || {
        store.take_wave(store_level, wave_idx)
    })?;
    encode_buckets(
        ctx.gzip_cache,
        (bucket_members(members, wave), None),
        ctx.zoom,
        &TileEncoding::single(ctx.opts).with_floors(ctx.floors),
        ctx.fills,
        ctx.timers,
        diagnostics,
    )
}

/// Route a wave's members to their partitions, one bucket per partition, as
/// [`finish_group`] routes them.
///
/// Each bucket is sized to its members first: grown by pushes, the buckets
/// held up to twice the wave's member structs beside the wave's own vector
/// at the export's peak.
pub(super) fn bucket_members(members: Vec<Member>, wave: &[Partition]) -> Vec<Vec<Member>> {
    let mut counts = vec![0usize; wave.len()];
    for m in &members {
        counts[route_partition(wave, m.key)] += 1;
    }
    let mut buckets: Vec<Vec<Member>> = counts.into_iter().map(Vec::with_capacity).collect();
    for m in members {
        buckets[route_partition(wave, m.key)].push(m);
    }
    buckets
}

/// Encode a wave's partition buckets in parallel. The tiles come back in
/// ascending partition order. Each bucket's members are released from
/// `admitted`, when given, as the bucket is encoded ([`InFlight`]).
pub(super) fn encode_buckets(
    gzip_cache: &Arc<TileGzipCache>,
    (buckets, admitted): (Vec<Vec<Member>>, Option<&Admitted<'_>>),
    zoom: u8,
    opts: &TileEncoding<'_>,
    fills: Option<&coverage_fill::FillMap>,
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<Vec<EncodedTile>>, ExportError> {
    crate::tile_trace::wait("encode_buckets", |site| {
        buckets
            .into_par_iter()
            .enumerate()
            .map(|(part, members)| {
                site.part(part, || {
                    let n = members.len();
                    let tiles = encode_layer_members(
                        gzip_cache,
                        members,
                        zoom,
                        opts,
                        fills,
                        timers,
                        diagnostics,
                    );
                    if let Some(a) = admitted {
                        a.encoded(n, tiles.as_deref().unwrap_or_default());
                    }
                    tiles
                })
            })
            .collect::<Result<_, _>>()
    })
}
