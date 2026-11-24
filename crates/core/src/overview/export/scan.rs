use super::*;

/// Scan **every** level in a single streaming read of the file (issue #233).
///
/// Scanning level `k` independently reads its *render set* — in partitioning
/// mode the accumulating prefix `0..=end_k` (spec §5.1) — so a row group
/// belonging to level `j` is re-read and re-decoded by every finer level
/// `k >= j` whose prefix includes it: `Σ_k |prefix_k|` row-group decodes.
///
/// This reads each level's **own band once** (`Σ_j |band_j|` = every row group
/// exactly once via [`send_bands`]) and *fans*
/// every decoded feature to the accumulators of every level whose render set
/// includes its band — level `j`'s features contribute to levels `{j}`
/// (duplicating, self-contained bands) or `{j..N}` (partitioning prefix).
///
/// The returned `LevelScan`s are **byte-identical** to `N` independent per-level
/// scans: a level's bounds union is component-wise `min`/`max` and its per-tile
/// counts are integer sums, both order- **and** grouping-independent — so
/// splitting the prefix into per-band reads and merging in band order yields the
/// same bounds rectangle and the same `tile_counts` map (and hence the same
/// partition plan and the same archive). Peak memory is one batch's bboxes plus
/// the `N` per-level `O(#tiles)` count maps.
///
/// PERF (#730): each feature's bbox comes from the file's f64 bbox covering
/// when it has one ([`ScanBboxSource::for_reader`]), so the scan reads four
/// doubles a row instead of decoding every geometry. A covering row the
/// geometry path would treat differently (an empty geometry's ±inf sentinel,
/// a non-finite bound) abandons that pass and rescans from the geometry, so
/// such a file scans exactly as before; a file with no usable covering scans
/// from the geometry from the start.
pub(super) fn scan_all_levels(
    reader: &OverviewReader,
    band: BandRead<'_>,
    crs: Crs,
    opts: &ExportOptions,
    timers: &ExportTimers,
    mut vd: Option<&mut variable_depth::VdScan>,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<LevelScan>, ExportError> {
    let t_scan = Instant::now();
    let done = |scans: Vec<LevelScan>, from: &str| {
        log::info!(
            "[export] scan complete: {} levels, single read, bboxes from the {from}, {:.2}s",
            scans.len(),
            t_scan.elapsed().as_secs_f64()
        );
        scans
    };
    let source = ScanBboxSource::for_reader(reader);
    if matches!(source, ScanBboxSource::Covering { .. }) {
        if let Some(scans) = scan_all_levels_from(
            (reader, band),
            crs,
            opts,
            timers,
            &source,
            vd.as_deref_mut(),
            diagnostics,
        )? {
            return Ok(done(scans, "bbox covering"));
        }
        log::info!(
            "[export] the bbox covering has an empty or non-finite row; \
             rescanning from the geometry"
        );
        if let Some(vd) = vd.as_deref_mut() {
            vd.reset();
        }
    }
    let scans = scan_all_levels_from(
        (reader, band),
        crs,
        opts,
        timers,
        &ScanBboxSource::Geometry,
        vd,
        diagnostics,
    )?
    .expect("a geometry scan never abandons its pass");
    Ok(done(scans, crate::resolved_schema::DEFAULT_GEOMETRY_COLUMN))
}

/// Where [`scan_all_levels`] gets each feature's bbox (#730).
#[derive(Debug, Clone, PartialEq)]
pub(super) enum ScanBboxSource {
    /// The geometry column's f64 bbox covering
    /// ([`crate::resolved_schema::ResolvedSchema::f64_covering`]): the struct column's name and
    /// its `xmin, ymin, xmax, ymax` child names.
    ///
    /// DIVERGENCE FROM THE GEOMETRY PATH: the covering's writer (the
    /// `geoparquet` crate) takes the min/max over every ring's coordinates,
    /// while `geo`'s `bounding_rect()` reads only a polygon's exterior. The
    /// two differ only for an invalid polygon with a hole outside its shell,
    /// where the covering is the larger box: the scan then counts the feature
    /// in a few tiles it emits nothing to, and the archive's advertised bounds
    /// can grow to include the hole. Planning stays safe because the scan's
    /// counts still cover every key the clip emits. See `context/ARCHITECTURE.md`.
    Covering { column: String, names: [String; 4] },
    /// Decode the geometry column ([`decode_batch_bboxes`]).
    Geometry,
}

impl ScanBboxSource {
    /// The covering when the file declares a bit-exact f64 one for its
    /// geometry column, else the geometry.
    pub(super) fn for_reader(reader: &OverviewReader) -> Self {
        let schema = reader.schema();
        reader
            .resolved_schema()
            .f64_covering()
            .map_or(Self::Geometry, |(idx, names)| Self::Covering {
                column: schema.field(idx).name().clone(),
                names,
            })
    }
}

/// [`scan_all_levels`] from one bbox source. `Ok(None)` only from
/// [`ScanBboxSource::Covering`], when a covering row has no exact bbox
/// ([`covering_batch_bboxes`]) and the pass is abandoned.
pub(super) fn scan_all_levels_from(
    (reader, band): (&OverviewReader, BandRead<'_>),
    crs: Crs,
    opts: &ExportOptions,
    timers: &ExportTimers,
    source: &ScanBboxSource,
    mut vd: Option<&mut variable_depth::VdScan>,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Option<Vec<LevelScan>>, ExportError> {
    let num_levels = reader.num_levels();
    let partitioning = matches!(reader.mode(), Mode::Partitioning);
    let zooms: Vec<u8> = (0..num_levels)
        .map(|k| zoom_for_level(reader.meta(), k))
        .collect::<Result<_, _>>()?;
    let mut scans: Vec<LevelScan> = (0..num_levels)
        .map(|_| LevelScan {
            feature_count: 0,
            bounds: None,
            tile_counts: BTreeMap::new(),
        })
        .collect();

    // PERF (#535): same read/compute overlap as [`process_wave`] — the read
    // runs off the consumer's thread while the decode + sizing fans out
    // over the Rayon pool, so reading and sizing alternately on one thread left
    // the pool parked for the whole read. The scan was 3.9 of the 16 s export
    // wall on a 3.07 M-feature z0–13 run. Every band goes down one channel in
    // band order (as [`fill_member_store`] does), so variable depth folds
    // batches in the order the serial loop did; the `LevelScan`s are sums
    // and min/max, identical however the consumer groups the rows.
    //
    // Memory (#559 review): the scan needs only each feature's bbox, so the
    // read is projected to the one column the bbox comes from — the covering
    // struct, else the geometry — and the pipe is the shallow
    // [`WAVE_READ_IN_FLIGHT`]: this runs before any wave preflight, so its
    // in-flight batches are unbudgeted (shortcut: under `auto` up to four
    // readers each hold [`BAND_READ_AHEAD_BATCHES`] of them too; charge a
    // `grant_read_ahead` if a geometry-column scan shows in peak RSS). Projection keeps every row and the
    // column's field (name and metadata), so the bboxes — and the scan — are
    // unchanged. With no recognizable geometry column the read stays
    // unprojected, so the error surfaces from the decode exactly as before.
    let geometry = geometry_name(reader);
    let root = match source {
        ScanBboxSource::Covering { column, .. } => reader.schema().index_of(column).ok(),
        ScanBboxSource::Geometry => reader.resolved_schema().geometry_index(),
    };
    // #803: rule A′ also reads the cluster and release columns, and a mixed
    // band's geometry for its rows' kinds.
    let roots: Option<Vec<usize>> = root
        .map(|r| -> Result<Vec<usize>, ExportError> {
            let mut roots = vec![r];
            if let Some(vd) = vd.as_deref() {
                for name in vd.columns() {
                    roots.push(variable_depth::column_index(reader.schema(), name)?);
                }
                if vd.needs_geometry() {
                    roots.extend(reader.resolved_schema().geometry_index());
                }
            }
            roots.sort_unstable();
            roots.dedup();
            Ok(roots)
        })
        .transpose()?;
    let scans_ref = &mut scans;
    let zooms_ref = &zooms;
    let mut abandoned = false;
    let abandoned_ref = &mut abandoned;
    #[cfg(test)]
    let mut readers = 0;
    #[cfg(test)]
    let readers_ref = &mut readers;
    // PERF (#817): each band is read by the ordered parallel reader, in
    // the same batches one reader makes; `bounded` keeps the one reader.
    scoped_pipe(
        WAVE_READ_IN_FLIGHT,
        |tx: &Sender<(usize, RecordBatch)>| -> Result<(), ExportError> {
            send_bands(
                (reader, band),
                roots.as_deref(),
                ReadPass::Scan,
                |read| add_nanos(&timers.scan_read, read),
                tx,
                #[cfg(test)]
                readers_ref,
            )
        },
        |rx: Receiver<(usize, RecordBatch)>| -> Result<(), ExportError> {
            // PERF (#817): one band's bboxes are sized a pool's worth at a
            // time, into each target level at once; one 8,192-row batch at a
            // time, level after level, kept the consumer to about one core.
            // Counts, unions and feature counts are sums and min/max, so the
            // scans are identical however rows are grouped.
            // `bounded` sizes batch by batch: a pool's worth at once is
            // outside its scan's memory envelope.
            let size_rows = match band.profile {
                MemoryProfile::Bounded => EXPORT_BATCH_SIZE,
                MemoryProfile::Auto | MemoryProfile::Speed => {
                    SIZE_BBOXES_MIN_ROWS.saturating_mul(rayon::current_num_threads())
                }
            };
            let mut pending: Vec<Option<TileBounds>> = Vec::with_capacity(size_rows);
            let mut pending_band = 0;
            let mut size_pending = |j: usize, pending: &mut Vec<Option<TileBounds>>| {
                if pending.is_empty() {
                    return;
                }
                #[cfg(test)]
                SCAN_SIZE_FLUSHES.with(|n| n.set(n.get() + 1));
                // Bands accumulate into finer levels' render sets only in
                // partitioning mode; duplicating bands are self-contained, so
                // band `j` feeds level `j` alone.
                let last = if partitioning { num_levels - 1 } else { j };
                ExportTimers::time(&timers.scan_size, || {
                    scans_ref[j..=last]
                        .par_iter_mut()
                        .zip(&zooms_ref[j..=last])
                        .for_each(|(scan, &zoom)| {
                            scan.feature_count += pending.len();
                            let (bounds, counts) = size_bboxes(pending, zoom, opts);
                            if let Some(b) = bounds {
                                match &mut scan.bounds {
                                    Some(acc) => acc.expand(&b),
                                    None => scan.bounds = Some(b),
                                }
                            }
                            for (key, v) in counts {
                                *scan.tile_counts.entry(key).or_insert(0) += v;
                            }
                        });
                });
                pending.clear();
            };
            for (j, batch) in rx.iter() {
                // Read + (for 3857) reproject each feature's bbox once per
                // band; sizing the same bbox into every target level below is
                // bit-identical to reprojecting it once per level, since
                // reprojection is a pure fn.
                let bboxes = ExportTimers::time(&timers.scan_decode, || match source {
                    ScanBboxSource::Covering { column, names } => {
                        covering_batch_bboxes(&batch, column, names, crs)
                    }
                    ScanBboxSource::Geometry => {
                        decode_batch_bboxes(&batch, geometry, crs, diagnostics).map(Some)
                    }
                })?;
                let Some(bboxes) = bboxes else {
                    // Dropping the receiver stops the producer cleanly.
                    *abandoned_ref = true;
                    return Ok(());
                };
                if let Some(vd) = vd.as_deref_mut() {
                    let geometries = || -> Result<Vec<Option<Geometry<f64>>>, ExportError> {
                        let geom_idx = batch_geometry_index(&batch, geometry)?;
                        let field = batch.schema().field(geom_idx).clone();
                        let garr = from_arrow_array(batch.column(geom_idx).as_ref(), &field)
                            .map_err(|e| {
                                crate::GeoParquetReadError::decode("geometry decode", e)
                            })?;
                        let mut out = Vec::with_capacity(batch.num_rows());
                        // Quiet: without variable depth the scan counts these
                        // rows' decode warnings only in its own bbox decode.
                        crate::batch_processor::extract_geometries_opt_from_array(
                            garr.as_ref(),
                            &mut out,
                            &crate::diagnostics::Diagnostics::silent(),
                        )?;
                        Ok(out)
                    };
                    let aligned = || -> Result<Vec<Option<TileBounds>>, ExportError> {
                        match source {
                            ScanBboxSource::Covering { column, names } => {
                                covering_row_bboxes(&batch, column, names, crs)
                            }
                            ScanBboxSource::Geometry => Ok(geometries()?
                                .iter()
                                .map(|g| g.as_ref().and_then(|g| geometry_lnglat_bbox(g, crs)))
                                .collect()),
                        }
                    };
                    ExportTimers::time(&timers.scan_size, || {
                        vd.fold(
                            j,
                            &batch,
                            &bboxes,
                            &variable_depth::RowSource {
                                bboxes: &aligned,
                                geometries: &geometries,
                            },
                            crs,
                            opts,
                        )
                    })?;
                }
                if j != pending_band {
                    size_pending(pending_band, &mut pending);
                    pending_band = j;
                }
                pending.extend(bboxes);
                if pending.len() >= size_rows {
                    size_pending(j, &mut pending);
                }
            }
            size_pending(pending_band, &mut pending);
            Ok(())
        },
    )?;
    #[cfg(test)]
    BAND_READ_WORKERS.with(|w| w.set(readers));
    Ok((!abandoned).then_some(scans))
}

/// Read-ahead per reader of [`send_bands`], in batches: the scan's
/// projected batches are small and its consumer keeps up, and the fill's
/// consumer takes one batch at a time, so the floor is enough.
pub(super) const BAND_READ_AHEAD_BATCHES: usize = 4;

#[cfg(test)]
thread_local! {
    /// Most reader threads one band read of this thread's last scan or
    /// single-read fill used (test counter, #817, #833).
    pub(super) static BAND_READ_WORKERS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Sizing passes this thread's scans ran, summed (test counter, #817).
    pub(super) static SCAN_SIZE_FLUSHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// One record batch's per-feature bboxes read from its f64 bbox covering
/// struct `column` (children `names`, in `xmin, ymin, xmax, ymax` order),
/// reprojected exactly as [`decode_batch_bboxes`] reprojects a geometry's.
///
/// The result matches [`decode_batch_bboxes`] on the same rows: a null
/// covering row is a null geometry, which the geometry path drops, so it is
/// dropped here too; every other row is the min/max over the geometry's own
/// coordinates (the covering is generated from the array the writer encodes,
/// in the file's CRS, unrounded). `Ok(None)` when any non-null row is not a
/// finite, ordered box — an empty geometry's `[+inf, +inf, -inf, -inf]`, a
/// NaN, a null child — since the geometry path's answer for those (no bbox
/// but counted, or not counted at all for an empty point) cannot be told
/// from the covering alone.
pub(super) fn covering_batch_bboxes(
    batch: &RecordBatch,
    column: &str,
    names: &[String; 4],
    crs: Crs,
) -> Result<Option<Vec<Option<TileBounds>>>, ExportError> {
    Ok(covering_rows(batch, column, names, crs)?
        .filter(|row| !matches!(row, Some(None)))
        .collect())
}

/// [`covering_batch_bboxes`] row-aligned: `None` for a null row. Only called
/// on a batch whose covering rows all passed that function's checks; a row
/// that does not is an error.
pub(super) fn covering_row_bboxes(
    batch: &RecordBatch,
    column: &str,
    names: &[String; 4],
    crs: Crs,
) -> Result<Vec<Option<TileBounds>>, ExportError> {
    covering_rows(batch, column, names, crs)?
        .collect::<Option<_>>()
        .ok_or_else(|| {
            crate::GeoParquetReadError::decode(
                "bbox covering",
                format!("{column:?} has a malformed row"),
            )
            .into()
        })
}

/// Each row of a bbox covering: `Some(None)` for a null row, `Some(bbox)`
/// for a finite, ordered box, `None` for anything else.
pub(super) fn covering_rows<'a>(
    batch: &'a RecordBatch,
    column: &str,
    names: &[String; 4],
    crs: Crs,
) -> Result<impl Iterator<Item = Option<Option<TileBounds>>> + 'a, ExportError> {
    let not_covering = || {
        crate::Error::from(crate::GeoParquetReadError::CoveringNotF64Struct {
            column: column.to_string(),
        })
    };
    let strukt = batch
        .column_by_name(column)
        .and_then(|c| c.as_struct_opt())
        .ok_or_else(not_covering)?;
    let child = |name: &String| {
        strukt
            .column_by_name(name)
            .and_then(|c| c.as_primitive_opt::<Float64Type>())
            .ok_or_else(not_covering)
    };
    let [xmin, ymin, xmax, ymax] = [
        child(&names[0])?,
        child(&names[1])?,
        child(&names[2])?,
        child(&names[3])?,
    ];
    Ok((0..strukt.len()).map(move |i| {
        if strukt.is_null(i) {
            return Some(None);
        }
        if [xmin, ymin, xmax, ymax].iter().any(|c| c.is_null(i)) {
            return None;
        }
        let (x0, y0, x1, y1) = (xmin.value(i), ymin.value(i), xmax.value(i), ymax.value(i));
        // Finite and ordered: `<=` is false for NaN, and an ordered pair with
        // one infinite end cannot pass the finiteness check.
        (x0 <= x1
            && y0 <= y1
            && x0.is_finite()
            && x1.is_finite()
            && y0.is_finite()
            && y1.is_finite())
        .then(|| Some(lnglat_bbox(x0, y0, x1, y1, crs)))
    }))
}

/// A bbox in the file's CRS as lon/lat [`TileBounds`]: the two corners go
/// through [`webmerc_to_lnglat`] for an EPSG:3857 file. Reprojection is
/// monotone per axis, so this is bit-identical to the bbox of the
/// reprojected geometry. Both scan sources go through here.
pub(crate) fn lnglat_bbox(x0: f64, y0: f64, x1: f64, y1: f64, crs: Crs) -> TileBounds {
    if matches!(crs, Crs::Epsg3857) {
        let (lng_min, lat_min) = webmerc_to_lnglat(x0, y0);
        let (lng_max, lat_max) = webmerc_to_lnglat(x1, y1);
        TileBounds::new(lng_min, lat_min, lng_max, lat_max)
    } else {
        TileBounds::new(x0, y0, x1, y1)
    }
}

/// A geometry's bounding box as lon/lat [`TileBounds`] (see
/// [`lnglat_bbox`]); `None` when it has none (empty).
pub(in crate::overview) fn geometry_lnglat_bbox(
    geom: &Geometry<f64>,
    crs: Crs,
) -> Option<TileBounds> {
    geom.bounding_rect()
        .map(|r| lnglat_bbox(r.min().x, r.min().y, r.max().x, r.max().y, crs))
}

/// Decode one record batch's geometries into per-feature bounding boxes,
/// already reprojected to lon/lat (EPSG:4326) when the file is EPSG:3857 —
/// element `i` is `None` when feature `i` has no bounding rectangle
/// (empty/degenerate). Reprojecting the two bbox corners is monotone per axis,
/// hence bit-identical to the bbox of the reprojected geometry.
pub(super) fn decode_batch_bboxes(
    batch: &RecordBatch,
    geometry: Option<&str>,
    crs: Crs,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<Option<TileBounds>>, ExportError> {
    let schema = batch.schema();
    let geom_idx = batch_geometry_index(batch, geometry)?;
    let geom_field = schema.field(geom_idx).clone();
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::GeoParquetReadError::decode("geometry decode", e))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(garr.as_ref(), &mut geoms, diagnostics)?;
    let bboxes = geoms
        .par_iter()
        .map(|geom| geometry_lnglat_bbox(geom, crs))
        .collect();
    Ok(bboxes)
}

/// Every tile key one feature with `bbox` is a member of at `zoom`, once each.
/// Counts must match what `feature_tile_members` will emit, so both go
/// through `wrap_shifts` and `member_ranges`.
#[inline]
pub(super) fn for_each_member_key(
    bbox: &TileBounds,
    zoom: u8,
    opts: &ExportOptions,
    mut f: impl FnMut(u64),
) {
    let buffer = buffer_deg_at_zoom(zoom, opts);
    if !near_world_edge(bbox, buffer) {
        let ranges = member_ranges(bbox, zoom, opts);
        for tc in tiles_in_ranges(&ranges, zoom) {
            f(tile_key(tc.x, tc.y, zoom));
        }
        return;
    }
    let copies: Vec<BboxTileRanges> = wrap_shifts(bbox, buffer)
        .map(|shift| member_ranges(&shift_bbox(bbox, shift), zoom, opts))
        .collect();
    for (i, ranges) in copies.iter().enumerate() {
        for tc in tiles_in_ranges(ranges, zoom) {
            // A tile two copies share is one member (#342).
            if copies[..i].iter().any(|r| ranges_contain(r, tc)) {
                continue;
            }
            f(tile_key(tc.x, tc.y, zoom));
        }
    }
}

/// Fewest bboxes one [`size_bboxes`] task sizes. Sizing a bbox costs well
/// under a microsecond, so finer splits only buy accumulator maps to merge
/// and a pool to wake: splitting a batch to the thread count leaves a large
/// pool spinning in Rayon (#817).
pub(super) const SIZE_BBOXES_MIN_ROWS: usize = 2048;

#[cfg(test)]
thread_local! {
    /// [`size_bboxes`] accumulators this thread started (test counter, #817).
    pub(super) static SIZE_ACCUMULATORS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Size a batch of (already-reprojected) feature bboxes at `zoom`: union their
/// bounds and count one member per tile each spans. Sizing is embarrassingly
/// parallel per feature (issue #227): fold into per-thread `(bounds, counts)`
/// accumulators, then reduce. Both reductions (bbox union, per-tile count sum)
/// are commutative and associative, so the merged result is identical
/// regardless of thread scheduling or how features were grouped into batches.
pub(super) fn size_bboxes(
    bboxes: &[Option<TileBounds>],
    zoom: u8,
    opts: &ExportOptions,
) -> (Option<TileBounds>, HashMap<u64, usize>) {
    bboxes
        .par_iter()
        .with_min_len(SIZE_BBOXES_MIN_ROWS)
        .fold(
            || {
                #[cfg(test)]
                SIZE_ACCUMULATORS.with(|n| n.set(n.get() + 1));
                (None::<TileBounds>, HashMap::<u64, usize>::new())
            },
            |(mut bounds, mut counts), bbox| {
                if let Some(bbox) = bbox {
                    for extent in wrapped_extents(bbox) {
                        match &mut bounds {
                            Some(acc) => acc.expand(&extent),
                            None => bounds = Some(extent),
                        }
                    }
                    // The accumulated `bounds` above stay the true (wrapped)
                    // extent, since they become the archive's advertised bbox.
                    for_each_member_key(bbox, zoom, opts, |key| {
                        *counts.entry(key).or_insert(0) += 1;
                    });
                }
                (bounds, counts)
            },
        )
        .reduce(
            || (None::<TileBounds>, HashMap::<u64, usize>::new()),
            |(b_a, c_a), (b_b, c_b)| {
                let bounds = match (b_a, b_b) {
                    (Some(mut a), Some(b)) => {
                        a.expand(&b);
                        Some(a)
                    }
                    (Some(a), None) | (None, Some(a)) => Some(a),
                    (None, None) => None,
                };
                // Merge the smaller map into the larger to minimize rehashing.
                let (mut dst, src) = if c_a.len() >= c_b.len() {
                    (c_a, c_b)
                } else {
                    (c_b, c_a)
                };
                for (k, v) in src {
                    *dst.entry(k).or_insert(0) += v;
                }
                (bounds, dst)
            },
        )
}

/// The parts of a feature with `bbox` that lie inside the ±180° longitude
/// domain once wrapped (#342): its in-domain part as stored, plus each ±360°
/// copy that overlaps the domain with positive width. A bbox already inside
/// the domain comes back unchanged. These are what the archive advertises, so
/// a cell stored as `x: 178 … 181` reports `[178, 180]` and `[-180, -179]`,
/// never lng 181.
pub(super) fn wrapped_extents(bbox: &TileBounds) -> impl Iterator<Item = TileBounds> + '_ {
    wrap_shifts(bbox, 0.0).map(move |shift| {
        let lo = bbox.lng_min + shift;
        let hi = bbox.lng_max + shift;
        TileBounds::new(lo.max(-180.0), bbox.lat_min, hi.min(180.0), bbox.lat_max)
    })
}

/// `true` when tile `tc` lies in `ranges`.
#[inline]
pub(super) fn ranges_contain(ranges: &BboxTileRanges, tc: TileCoord) -> bool {
    let within = |(lo, hi): (u32, u32), v: u32| lo <= v && v <= hi;
    within(ranges.y, tc.y)
        && (within(ranges.x, tc.x) || ranges.x2.is_some_and(|x2| within(x2, tc.x)))
}

/// Per-level scan oracle: reads level `level_idx`'s full render set (the
/// mode-dependent prefix, via [`OverviewReader::read_level`])
/// and sizes it at `zoom`. This is the pre-#233 independent-per-level read path,
/// retained as the byte-identity oracle for [`scan_all_levels`] — a genuinely
/// different read (one prefix vs a fan of per-band reads) that must yield the
/// same `LevelScan`.
#[cfg(test)]
pub(super) fn scan_level(
    reader: &OverviewReader,
    level_idx: usize,
    crs: Crs,
    zoom: u8,
    opts: &ExportOptions,
) -> Result<LevelScan, ExportError> {
    let batch_reader = reader.read_level(level_idx, None)?;
    let mut scan = LevelScan {
        feature_count: 0,
        bounds: None,
        tile_counts: BTreeMap::new(),
    };
    for batch in batch_reader {
        let bboxes = decode_batch_bboxes(
            &batch.map_err(|e| read_error(e.into()))?,
            geometry_name(reader),
            crs,
            &crate::diagnostics::Diagnostics::default(),
        )?;
        scan.feature_count += bboxes.len();
        let (batch_bounds, batch_counts) = size_bboxes(&bboxes, zoom, opts);
        if let Some(b) = batch_bounds {
            match &mut scan.bounds {
                Some(acc) => acc.expand(&b),
                None => scan.bounds = Some(b),
            }
        }
        for (k, v) in batch_counts {
            *scan.tile_counts.entry(k).or_insert(0) += v;
        }
    }
    Ok(scan)
}

/// Split a zoom's tiles (ascending key order — since #506, ascending PMTiles
/// tile id / Hilbert order) into contiguous partitions of at least
/// `partition_target` members each (the last partition may be smaller).
///
/// Because Hilbert order is spatially compact rather than column-striped, a
/// partition's bbox (recomputed below as the union of its member tiles'
/// bounds, same as before) is now typically a blocky region instead of a tall
/// thin strip — which tends to prune more row groups on the wave read, a
/// measurable side effect of this key change rather than something planned
/// for here.
pub(super) fn plan_partitions(
    tile_counts: &BTreeMap<u64, usize>,
    zoom: u8,
    partition_target: usize,
) -> Vec<Partition> {
    let target = partition_target.max(1);
    let mut out: Vec<Partition> = Vec::new();
    let mut cur: Option<Partition> = None;
    for (&key, &count) in tile_counts {
        let (x, y) = key_to_xy(key, zoom);
        let tb = TileCoord::new(x, y, zoom).bounds();
        match cur.as_mut() {
            Some(p) => {
                p.key_hi = key;
                p.bbox.add(&tb);
                p.members += count;
            }
            None => {
                cur = Some(Partition {
                    key_lo: key,
                    key_hi: key,
                    bbox: PruneRegion::of(tb),
                    members: count,
                });
            }
        }
        if cur.as_ref().is_some_and(|p| p.members >= target) {
            out.push(cur.take().unwrap());
        }
    }
    out.extend(cur);
    // One-tile margin on the pruning bbox: generously covers any rounding in
    // row-group bbox statistics; over-inclusion only reads an extra row group.
    let margin = 360.0 / 2f64.powi(zoom as i32);
    for p in &mut out {
        p.bbox.grow(margin);
    }
    out
}
