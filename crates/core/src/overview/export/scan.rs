use super::*;
use valve_predict::FLUSH_ROWS;

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
#[expect(
    clippy::too_many_arguments,
    reason = "#768: the valve predictor rides along like variable depth"
)]
pub(super) fn scan_all_levels(
    reader: &OverviewReader,
    band: BandRead<'_>,
    crs: Crs,
    opts: &ExportOptions,
    timers: &ExportTimers,
    mut vd: Option<&mut variable_depth::VdScan>,
    mut predict: Option<&mut valve_predict::Scan>,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<LevelScan>, ExportError> {
    let t_scan = Instant::now();
    let done = |scans: Vec<LevelScan>, from: &str| {
        log::info!(target: LOG_TARGET,
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
            predict.as_deref_mut(),
            diagnostics,
        )? {
            return Ok(done(scans, "bbox covering"));
        }
        log::info!(target: LOG_TARGET,
            "[export] the bbox covering has an empty or non-finite row; \
             rescanning from the geometry"
        );
        if let Some(vd) = vd.as_deref_mut() {
            vd.reset();
        }
        if let Some(p) = predict.as_deref_mut() {
            p.reset();
        }
    }
    let scans = scan_all_levels_from(
        (reader, band),
        crs,
        opts,
        timers,
        &ScanBboxSource::Geometry,
        vd,
        predict,
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
#[expect(
    clippy::too_many_arguments,
    reason = "#768: the valve predictor rides along like variable depth"
)]
pub(super) fn scan_all_levels_from(
    (reader, band): (&OverviewReader, BandRead<'_>),
    crs: Crs,
    opts: &ExportOptions,
    timers: &ExportTimers,
    source: &ScanBboxSource,
    mut vd: Option<&mut variable_depth::VdScan>,
    mut predict: Option<&mut valve_predict::Scan>,
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
    // #768 predict: the gap walk rides in the consumer, fed each batch's
    // row-aligned bboxes in order. Not on a thread of its own: its Rayon
    // work could then wait on the only pool worker, held by this scan.
    // PERF (#768): the predictor counts every tile's members already, so the
    // sizing below keeps only the bounds and takes its counts at the end.
    let count_tiles = predict.is_none();
    // PERF (#768): the predictor's flush holds the consumer; a pipe one
    // flush deep keeps the band read going meanwhile. Covering batches are
    // four f64 per row, so it holds a few MiB.
    let depth = match source {
        ScanBboxSource::Covering { .. } if predict.is_some() => FLUSH_ROWS / EXPORT_BATCH_SIZE,
        _ => WAVE_READ_IN_FLIGHT,
    };
    let predict_ref = &mut predict;
    // PERF (#817): each band is read by the ordered parallel reader, in
    // the same batches one reader makes; `bounded` keeps the one reader.
    let piped = scoped_pipe(
        depth,
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
                            let (bounds, counts) = size_bboxes(pending, zoom, opts, count_tiles);
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
                // A geometry batch with a null row: its rows row-aligned too.
                let mut aligned: Option<Bboxes> = None;
                let bboxes = ExportTimers::time(&timers.scan_decode, || match source {
                    ScanBboxSource::Covering { column, names } => {
                        covering_batch_bboxes(&batch, column, names, crs)
                    }
                    ScanBboxSource::Geometry => {
                        let (sized, rows) =
                            decode_batch_bboxes(&batch, geometry, crs, diagnostics)?;
                        aligned = rows;
                        Ok(Some(sized))
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
                            // Row-aligned, `None` for a null row. Every row passed
                            // `covering_batch_bboxes`' checks, so one that does not
                            // is an error.
                            ScanBboxSource::Covering { column, names } => {
                                covering_rows(&batch, column, names, crs)?
                                    .collect::<Option<_>>()
                                    .ok_or_else(|| {
                                        crate::GeoParquetReadError::decode(
                                            "bbox covering",
                                            format!("{column:?} has a malformed row"),
                                        )
                                        .into()
                                    })
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
                if let Some(p) = predict_ref.as_deref_mut() {
                    // #768 predict: row-aligned, so a tile's sample rows are its band rows.
                    // A covering batch with a null row, all checked above, is read again.
                    let last = if partitioning { num_levels - 1 } else { j };
                    let kept;
                    let rows = match source {
                        ScanBboxSource::Covering { column, names }
                            if bboxes.len() != batch.num_rows() =>
                        {
                            kept = covering_rows(&batch, column, names, crs)?
                                .map(Option::flatten)
                                .collect::<Vec<_>>();
                            &kept[..]
                        }
                        _ => aligned.as_deref().unwrap_or(&bboxes),
                    };
                    p.batch(j, rows, j..=last, zooms_ref, opts);
                    shed_over_budget(p, (&band.budget, band.profile), opts, diagnostics);
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
    );
    piped?;
    // The tile counts stay in the predictor until its floors are built
    // ([`scan_with_floors`]).
    if let Some(p) = predict {
        p.flush(zooms_ref, opts);
        shed_over_budget(p, (&band.budget, band.profile), opts, diagnostics);
    }
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

/// Per-row bboxes, `None` for a row without one.
type Bboxes = Vec<Option<TileBounds>>;

/// Decode one record batch's geometries into per-feature bounding boxes,
/// already reprojected to lon/lat (EPSG:4326) when the file is EPSG:3857 —
/// element `i` is `None` when feature `i` has no bounding rectangle
/// (empty/degenerate). Reprojecting the two bbox corners is monotone per axis,
/// hence bit-identical to the bbox of the reprojected geometry.
///
/// With them, when a null geometry (which they drop) makes the two differ,
/// the same rows row-aligned, `None` for a null row (#768: the valve
/// predictor addresses a tile's sample rows by their place in the file).
pub(super) fn decode_batch_bboxes(
    batch: &RecordBatch,
    geometry: Option<&str>,
    crs: Crs,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(Bboxes, Option<Bboxes>), ExportError> {
    let schema = batch.schema();
    let geom_idx = batch_geometry_index(batch, geometry)?;
    let geom_field = schema.field(geom_idx).clone();
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::GeoParquetReadError::decode("geometry decode", e))?;
    let mut geoms: Vec<Option<Geometry<f64>>> = Vec::with_capacity(batch.num_rows());
    crate::batch_processor::extract_geometries_opt_from_array(
        garr.as_ref(),
        &mut geoms,
        diagnostics,
    )?;
    let rows: Bboxes = geoms
        .par_iter()
        .map(|geom| geom.as_ref().and_then(|g| geometry_lnglat_bbox(g, crs)))
        .collect();
    if geoms.iter().all(Option::is_some) {
        return Ok((rows, None));
    }
    let sized = rows
        .iter()
        .zip(&geoms)
        .filter_map(|(b, g)| g.as_ref().map(|_| *b))
        .collect();
    Ok((sized, Some(rows)))
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

/// At least as many tiles as [`for_each_member_key`] calls back for: the
/// same copies, a tile two copies share counted twice. Cheap, so a caller
/// can bound what it holds per member before it enumerates them.
pub(super) fn member_tiles_bound(bbox: &TileBounds, zoom: u8, opts: &ExportOptions) -> u64 {
    let tiles = |b: &TileBounds| {
        let r = member_ranges(b, zoom, opts);
        let width = |(lo, hi): (u32, u32)| u64::from(hi - lo) + 1;
        (width(r.x) + r.x2.map_or(0, width)) * width(r.y)
    };
    let buffer = buffer_deg_at_zoom(zoom, opts);
    if !near_world_edge(bbox, buffer) {
        return tiles(bbox);
    }
    wrap_shifts(bbox, buffer)
        .map(|shift| tiles(&shift_bbox(bbox, shift)))
        .sum()
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
/// Without `count`, only the bounds.
pub(super) fn size_bboxes(
    bboxes: &[Option<TileBounds>],
    zoom: u8,
    opts: &ExportOptions,
    count: bool,
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
                    // PERF (#768): the predictor counts the tiles itself.
                    if count {
                        for_each_member_key(bbox, zoom, opts, |key| {
                            *counts.entry(key).or_insert(0) += 1;
                        });
                    }
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
        let (bboxes, _) = decode_batch_bboxes(
            &batch.map_err(|e| read_error(e.into()))?,
            geometry_name(reader),
            crs,
            &crate::diagnostics::Diagnostics::default(),
        )?;
        scan.feature_count += bboxes.len();
        let (batch_bounds, batch_counts) = size_bboxes(&bboxes, zoom, opts, true);
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

/// The export scan ([`scan_all_levels`]), with the #768 valve predictor
/// riding along under `--valve-scope smooth|zoom`, and the floors it
/// predicts. The predictor's state is charged to `budget` while the floors
/// are priced and returned once they are built; the scan's tile counts,
/// which the predictor holds, are copied out after.
pub(super) fn scan_with_floors(
    reader: &OverviewReader,
    crs: Crs,
    cols: (&PublishedNames, Option<&ResolvedFeatureId>),
    (options, level_zooms): (&ExportOptions, &[u8]),
    budget: &mut MemoryBudget,
    (timers, vd): (&ExportTimers, Option<&mut variable_depth::VdScan>),
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(Vec<LevelScan>, valve_predict::ValveFloors), ExportError> {
    let mut predict =
        valve_predicts(options).then(|| valve_predict::Scan::new(reader.num_levels()));
    let band = BandRead {
        source: reader.source(),
        budget: *budget,
        profile: options.profile,
    };
    let mut scans = scan_all_levels(
        reader,
        band,
        crs,
        options,
        timers,
        vd,
        predict.as_mut(),
        diagnostics,
    )?;
    let Some(p) = predict else {
        return Ok((scans, valve_predict::ValveFloors::default()));
    };
    log::info!(
        "[export] valve predictor state: {:.1} MiB",
        p.resident_bytes() as f64 / f64::from(1 << 20)
    );
    let held = budget.reserve_up_to(MemoryPhase::Scan, p.resident_bytes());
    let floors = if p.is_shed() {
        Ok(valve_predict::ValveFloors::default())
    } else {
        predict_floors(reader, cols, crs, (options, level_zooms), &p, budget).map(|f| {
            f.unwrap_or_else(|| {
                warn_floors_shed(false, options, diagnostics);
                valve_predict::ValveFloors::default()
            })
        })
    };
    budget.unreserve(MemoryPhase::Scan, held);
    for (scan, counts) in scans.iter_mut().zip(p.into_tile_counts()) {
        scan.tile_counts = counts;
    }
    let floors = floors?;
    // Held through pass 2: every tile reads its floor there.
    budget.reserve_up_to(MemoryPhase::Scan, floors.resident_bytes());
    Ok((scans, floors))
}

/// #768: say loudly (the diagnostic logs itself) that the `--valve-scope`
/// predictor's scan state, or the floors it predicted, did not fit the
/// export's memory budget, and that the export runs as `tile`.
fn warn_floors_shed(
    predictor: bool,
    opts: &ExportOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) {
    diagnostics.record(crate::diagnostics::Diagnostic::ValveFloorsShed {
        count: 1,
        scope: opts.valve_scope.to_string(),
        predictor,
    });
}

/// #768: drop the predictor's dense state when it outgrows what the scan
/// may hold under `band`'s budget, and say so: the export goes on with the
/// tile counts alone, as under `--valve-scope tile`.
pub(super) fn shed_over_budget(
    p: &mut valve_predict::Scan,
    (budget, profile): (&MemoryBudget, MemoryProfile),
    opts: &ExportOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) {
    if p.is_shed() {
        return;
    }
    let held = p.resident_bytes();
    if budget
        .require_allowance(profile, MemoryPhase::Scan, held)
        .is_err()
    {
        p.shed();
        log::info!("[export] valve predictor state: {held} bytes, over the scan's budget");
        warn_floors_shed(true, opts, diagnostics);
    }
}

/// #768 `--valve-scope smooth|zoom`: each tile's predicted need, priced by
/// real encodes of samples of its own members, and the floors they put on
/// the tiles ([`valve_predict::Scan::floors`]).
fn predict_floors(
    reader: &OverviewReader,
    cols: (&PublishedNames, Option<&ResolvedFeatureId>),
    crs: Crs,
    (options, level_zooms): (&ExportOptions, &[u8]),
    p: &valve_predict::Scan,
    budget: &MemoryBudget,
) -> Result<Option<valve_predict::ValveFloors>, ExportError> {
    let t = Instant::now();
    // A sampled member's resident bytes, as pass 2 prices one.
    let member = export_member_bytes(reader, cols.0, reader.finest_level_mean_row_bytes())
        .unwrap_or_else(|| budget.estimate(MemoryPhase::ExportCollect, 1, None));
    // Every tile that can be over is priced by a small sample of its own
    // members; those it puts near a cap, by a larger one. Tiles are sampled
    // a chunk at a time, in `(level, key)` order, each chunk's members freed
    // once encoded: as many tiles as the export's share of the budget holds.
    let sample =
        |tiles: &[valve_predict::SampleAt],
         members: usize,
         rows: &dyn Fn(&[valve_predict::SampleAt]) -> Result<SampleRows, ExportError>| {
            let price = Price::model(member.saturating_mul(members as u64));
            let chunk = budget.affordable(options.profile, MemoryPhase::ExportCollect, price);
            let mut bytes = HashMap::new();
            for tiles in tiles.chunks(chunk) {
                #[cfg(test)]
                SAMPLE_CHUNKS.with(|n| n.set(n.get() + 1));
                let b = valve_sample_bytes(
                    reader,
                    cols,
                    crs,
                    (options, level_zooms),
                    tiles,
                    rows(tiles)?,
                )?;
                bytes.extend(
                    tiles
                        .iter()
                        .zip(b)
                        .filter_map(|(t, b)| Some(((t.level, t.key), b?))),
                );
            }
            Ok::<_, ExportError>(bytes)
        };
    let priced = p.priced();
    let mut bytes = sample(&priced, valve_predict::PRICE_MEMBERS, &|tiles| {
        stored_sample_rows(reader, p, tiles)
    })?;
    let t_first = t.elapsed();
    let candidates = p.candidates(&bytes, options);
    bytes.extend(sample(&candidates, SAMPLE_MEMBERS, &|tiles| {
        valve_sample_rows(
            reader,
            cols.0,
            crs,
            (options, level_zooms),
            tiles,
            SAMPLE_MEMBERS,
        )
    })?);
    let need = p.predict(level_zooms, &bytes, options);
    // The floors stay resident through the export, so they get what the
    // budget has left beside the predictor's own state.
    let max_floors =
        usize::try_from(budget.room(options.profile) / valve_predict::FLOOR_ENTRY_BYTES)
            .unwrap_or(usize::MAX);
    let floors = valve_predict::valve_floors(&need, options, max_floors);
    log::info!(
        "[export] valve floors: {} tiles priced ({} by a larger sample, {} by neither: their rows clip empty), {} predicted over a cap, {} floors, {:.2}s (first sample {:.2}s; scan share {:.2}s)",
        bytes.len(),
        candidates.len(),
        need.unpriced,
        need.len(),
        floors.as_ref().map_or(0, valve_predict::ValveFloors::len),
        t.elapsed().as_secs_f64(),
        t_first.as_secs_f64(),
        p.spent.as_secs_f64(),
    );
    Ok(floors)
}

#[cfg(test)]
thread_local! {
    /// Chunks this thread's [`predict_floors`] sampled tiles in (test
    /// counter, #768).
    pub(super) static SAMPLE_CHUNKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// #768 predict: members in the larger price sample of a tile its first
/// one ([`valve_predict::PRICE_MEMBERS`]) puts near a cap. A sample that
/// clips to fewer than `MIN_SAMPLE_MEMBERS` pieces prices nothing: below
/// the first sample's rows, so a row whose bbox reaches the tile but whose
/// clip there is empty does not leave the tile unpriced.
const SAMPLE_MEMBERS: usize = 4096;
const MIN_SAMPLE_MEMBERS: usize = valve_predict::PRICE_MEMBERS * 3 / 4;

/// Per row group: each tile (its index) and its rows to sample there.
type SampleRows = BTreeMap<usize, Vec<(usize, Vec<u32>)>>;

/// #768: the rows the scan kept to price each of `tiles` by
/// ([`valve_predict::Scan::price_rows`]), per row group.
fn stored_sample_rows(
    reader: &OverviewReader,
    p: &valve_predict::Scan,
    tiles: &[valve_predict::SampleAt],
) -> Result<SampleRows, ExportError> {
    // Each band's row groups and the band row each starts at.
    let mut starts: HashMap<u16, Vec<(usize, u64)>> = HashMap::new();
    let mut chosen = SampleRows::new();
    for (i, t) in tiles.iter().enumerate() {
        for (band, row) in p.price_rows(t) {
            let groups = match starts.entry(band) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => {
                    let mut at = 0u64;
                    let rows_of = |rg| reader.parquet_metadata().row_group(rg).num_rows().max(0);
                    let groups = reader.band(usize::from(band))?.into_iter().map(|rg| {
                        let start = at;
                        at += rows_of(rg) as u64;
                        (rg, start)
                    });
                    e.insert(groups.collect())
                }
            };
            let (rg, start) = groups[groups.partition_point(|&(_, s)| s <= row) - 1];
            let entry = chosen.entry(rg).or_default();
            let offset = (row - start) as u32;
            match entry.last_mut() {
                Some((j, rows)) if *j == i => rows.push(offset),
                _ => entry.push((i, vec![offset])),
            }
        }
    }
    Ok(chosen)
}

/// #768: the rows [`valve_sample_bytes`] samples, per row group: each
/// tile's first `sample_members` member rows in its span, read from the
/// bbox covering alone (the geometry when there is none).
///
/// PERF: tiles in parallel, each walking its span's row groups in order and
/// stopping at its sample. Walking every row group of every span in
/// parallel read and scanned a coarse tile's whole band for a sample its
/// first row group already held (France points: 6 of 15 s of CPU).
fn valve_sample_rows(
    reader: &OverviewReader,
    published: &PublishedNames,
    crs: Crs,
    (opts, zooms): (&ExportOptions, &[u8]),
    tiles: &[valve_predict::SampleAt],
    sample_members: usize,
) -> Result<SampleRows, ExportError> {
    let rows_of = |rg: usize| reader.parquet_metadata().row_group(rg).num_rows().max(0) as u64;
    let source = ScanBboxSource::for_reader(reader);
    let projection: Option<Vec<usize>> = match &source {
        ScanBboxSource::Covering { .. } => reader
            .resolved_schema()
            .f64_covering()
            .map(|(idx, _)| vec![idx]),
        ScanBboxSource::Geometry => reader.resolved_schema().geometry_index().map(|i| vec![i]),
    };
    let diagnostics = crate::diagnostics::Diagnostics::default();
    // One tile's first `sample_members` member rows, per row group of its
    // span, in band order.
    let sample_tile = |t: &valve_predict::SampleAt| -> Result<Vec<(usize, Vec<u32>)>, ExportError> {
        let mut found: Vec<(usize, Vec<u32>)> = Vec::new();
        let mut left = sample_members;
        let mut start = 0u64;
        for rg in reader.band(usize::from(t.band))? {
            let n = rows_of(rg);
            // The span inside this row group, rows from its start.
            let (lo, hi) = (t.row.max(start), t.last.min((start + n).saturating_sub(1)));
            start += n;
            if n == 0 || lo > hi {
                if start > t.last {
                    break;
                }
                continue;
            }
            let (lo, hi) = (lo - (start - n), hi - (start - n));
            let mut rows = Vec::new();
            let mut base = 0u64;
            for batch in read_row_group(reader, rg, projection.as_deref())? {
                let batch = batch.map_err(|e| read_error(e.into()))?;
                let m = batch.num_rows() as u64;
                if lo < base + m && hi >= base {
                    let bboxes: Vec<Option<TileBounds>> = match &source {
                        // A malformed covering row (the scan then fell back
                        // to geometry) is sampled as no member.
                        ScanBboxSource::Covering { column, names } => {
                            covering_rows(&batch, column, names, crs)?
                                .map(Option::flatten)
                                .collect()
                        }
                        ScanBboxSource::Geometry => {
                            let (sized, rows) = decode_batch_bboxes(
                                &batch,
                                published.geometry.as_deref(),
                                crs,
                                &diagnostics,
                            )?;
                            rows.unwrap_or(sized)
                        }
                    };
                    for row in lo.max(base) - base..=hi.min(base + m - 1) - base {
                        let Some(Some(b)) = bboxes.get(row as usize) else {
                            continue;
                        };
                        let mut member = false;
                        for_each_member_key(b, zooms[t.level], opts, |k| member |= k == t.key);
                        if member {
                            rows.push((base + row) as u32);
                            if rows.len() == left {
                                break;
                            }
                        }
                    }
                }
                base += m;
                if base > hi || rows.len() == left {
                    break;
                }
            }
            left -= rows.len();
            if !rows.is_empty() {
                found.push((rg, rows));
            }
            if left == 0 || start > t.last {
                break;
            }
        }
        Ok(found)
    };
    let per_tile = tiles
        .par_iter()
        .map(sample_tile)
        .collect::<Result<Vec<_>, ExportError>>()?;
    let mut chosen = SampleRows::new();
    for (i, groups) in per_tile.into_iter().enumerate() {
        for (rg, rows) in groups {
            chosen.entry(rg).or_default().push((i, rows));
        }
    }
    Ok(chosen)
}

/// One row group of the overview, as batches, with `projection`.
fn read_row_group<'a>(
    reader: &'a OverviewReader,
    rg: usize,
    projection: Option<&[usize]>,
) -> Result<crate::input_set::SourceStream<'a>, ExportError> {
    let selection = RowGroupSelection::from_parts(vec![vec![rg]]);
    reader
        .source()
        .open_stream(&crate::input_set::ReadPlan {
            batch_size: tylertoo_io::DEFAULT_BATCH_ROWS,
            projection,
            row_groups: Some(&selection),
        })
        .map_err(|e| read_error(e.into()))
}

/// #768: the raw and gzipped MVT bytes one member of each of `tiles`
/// costs, from a real first encode of a sample of its members (`chosen`,
/// per row group): the export's own clip, attributes and encoder. `None`
/// for a tile whose sample is too small to price.
///
/// Which rows are sampled depends only on the tile's own members, never on
/// row positions, so a shard samples what the full build does. Row groups
/// read in parallel, each once, decoding only the chosen rows.
fn valve_sample_bytes(
    reader: &OverviewReader,
    (published, feature_id): (&PublishedNames, Option<&ResolvedFeatureId>),
    crs: Crs,
    (opts, zooms): (&ExportOptions, &[u8]),
    tiles: &[valve_predict::SampleAt],
    chosen: SampleRows,
) -> Result<Vec<Option<MemberBytes>>, ExportError> {
    let diagnostics = crate::diagnostics::Diagnostics::default();
    // Read 2: whole rows, only the chosen ones decoded.
    let timers = ExportTimers::default();
    let per_rg = chosen
        .into_par_iter()
        .map(
            |(rg, wanted)| -> Result<Vec<(usize, Vec<Member>)>, ExportError> {
                let band = reader.row_group_bands(&[rg])[0].0;
                let mut members: Vec<Vec<Member>> = wanted.iter().map(|_| Vec::new()).collect();
                let mut base = 0u64;
                let last = wanted
                    .iter()
                    .filter_map(|(_, r)| r.last())
                    .max()
                    .copied()
                    .unwrap_or(0);
                for batch in read_row_group(reader, rg, None)? {
                    let batch = batch.map_err(|e| read_error(e.into()))?;
                    let n = batch.num_rows() as u64;
                    let in_batch = |r: &&u32| u64::from(**r) >= base && u64::from(**r) < base + n;
                    let mut rows: Vec<u32> = wanted
                        .iter()
                        .flat_map(|(_, r)| r.iter().filter(in_batch))
                        .map(|&r| (u64::from(r) - base) as u32)
                        .collect();
                    rows.sort_unstable();
                    rows.dedup();
                    if !rows.is_empty() {
                        let taken = arrow_select::take::take_record_batch(
                            &batch,
                            &arrow_array::UInt32Array::from(rows.clone()),
                        )?;
                        let schema = taken.schema();
                        let gi = batch_geometry_index(&taken, published.geometry.as_deref())?;
                        let geoms = decode_batch_geometries(
                            &taken,
                            &schema,
                            gi,
                            crs,
                            &timers,
                            &diagnostics,
                        )?;
                        let mut attrs =
                            AttrColumns::new(&schema, gi, published, feature_id).extract(&taken)?;
                        // Built once per row: two tiles can sample the same row.
                        let props: Vec<SharedProps> =
                            (0..rows.len()).map(|k| attrs.props(k)).collect();
                        for (w, (i, wanted_rows)) in wanted.iter().enumerate() {
                            let t = tiles[*i];
                            for r in wanted_rows.iter().filter(in_batch) {
                                let at = (u64::from(*r) - base) as u32;
                                let k = rows.binary_search(&at).expect("taken above");
                                let pieces = feature_tile_members(
                                    &geoms[k],
                                    zooms[t.level],
                                    opts,
                                    t.key,
                                    t.key,
                                    &diagnostics,
                                );
                                let props = &props[k];
                                let id = attrs.id(k)?;
                                members[w].extend(pieces.into_iter().map(|(key, g)| Member {
                                    key,
                                    seq: u64::from(*r),
                                    rank: attrs.rank(k, u64::from(*r)),
                                    geom: g.into(),
                                    props: props.clone(),
                                    id,
                                    gap: coalesce_valve::GAP_NONE,
                                    band,
                                }));
                            }
                        }
                    }
                    base += n;
                    if base > u64::from(last) {
                        break;
                    }
                }
                Ok(wanted.iter().map(|(i, _)| *i).zip(members).collect())
            },
        )
        .collect::<Result<Vec<_>, ExportError>>()?;
    let mut samples: Vec<Vec<Member>> = tiles.iter().map(|_| Vec::new()).collect();
    for (i, m) in per_rg.into_iter().flatten() {
        samples[i].extend(m);
    }
    let cache = Arc::new(TileGzipCache::new(0));
    samples
        .into_par_iter()
        .zip(tiles)
        .map(|(mut sample, t)| {
            if sample.len() < MIN_SAMPLE_MEMBERS {
                return Ok(None);
            }
            let (x, y) = key_to_xy(t.key, zooms[t.level]);
            let tb = TileCoord::new(x, y, zooms[t.level]).bounds();
            // `--feature-order` ties go by the tile's own file order: a
            // row's place in its row group moves with a shard's row groups.
            for (k, m) in sample.iter_mut().enumerate() {
                m.seq = k as u64;
            }
            sort_tile_for_encode(&mut sample, &opts.feature_order);
            let mut mvt = build_tile_layers(&cache, &[], &sample, &tb, opts, &[], &diagnostics);
            let n = sample.len() as f64;
            let raw = mvt.data.len() as f64;
            let gz = mvt.data.gzipped(&cache)?.len() as f64;
            Ok(Some((raw / n, gz / n)))
        })
        .collect()
}

pub(super) use valve_predict::MemberBytes;
