use super::*;

/// Encodes [`with_fill`] may take to fit a #750 fill into a tile's slack
/// before it writes the tile without one.
pub(super) const MAX_FILL_PASSES: u32 = 4;

/// The tile the size valve fitted (`fitted`) with as much of its #750 fill
/// as fits in the room left, re-encoding the same features with a subset of
/// the fill through `encode` (the merge valve's cached encode, so a heavily
/// merged tile is not encoded the slow way again): all of it if the tile still
/// fits, else the largest fill features whose bytes fit the slack, else
/// none. A tile the valve could not fit gets no fill. Each extra encode
/// counts as a valve pass. A reduced grid or a dropped merge also sheds
/// fill: its old commands must not redraw a dropped merge's source.
pub(super) fn with_fill(
    mut fitted: TileMvt,
    opts: &ExportOptions,
    fill: &[FillFeature],
    tally: &mut FillTally,
    mut encode: impl FnMut(&[&FillFeature]) -> TileMvt,
) -> TileMvt {
    // Observe successful gzip-buffer replacement, rather than assuming a
    // fit check compressed. This guard can pass when the fit owner reuses
    // cached bytes, without changing the observer to predict that fix.
    #[cfg(test)]
    fn note_gzip(before: Option<*const u8>, mvt: &TileMvt) {
        let after = mvt.gzip.as_ref().map(|g| g.as_ptr());
        if after.is_some() && after != before {
            coverage_fill::cost_probe::update(|c| {
                c.fill_gzip_calls += 1;
                c.cached_gzip_rechecks += usize::from(before.is_some());
            });
        }
    }
    if fill.is_empty() {
        return fitted;
    }
    #[cfg(test)]
    let base_gzip_before = fitted.gzip.as_ref().map(|g| g.as_ptr());

    // Drawn and shed, from the fill features themselves.
    let total: usize = fill.iter().map(|f| f.polygons).sum();
    let no_fill = |tally: &mut FillTally, fitted: TileMvt| {
        tally.shed += total;
        tally.polygons = 0;
        tally.vertices = 0;
        fitted
    };
    if fitted.valve.reduced_extent.is_some()
        || fitted.valve.dropped_merged > 0
        || tile_fit(&mut fitted, opts).unwrap_or(f64::INFINITY) < 1.0
    {
        #[cfg(test)]
        note_gzip(base_gzip_before, &fitted);
        return no_fill(tally, fitted);
    }
    #[cfg(test)]
    note_gzip(base_gzip_before, &fitted);
    // Largest first; drawn in their own order.
    let mut by_area: Vec<usize> = (0..fill.len()).collect();
    by_area.sort_by(|&a, &b| fill[b].area.total_cmp(&fill[a].area).then(a.cmp(&b)));
    let mut keep = fill.len();
    let mut tried = 0;
    for pass in 1..=MAX_FILL_PASSES {
        tried = pass;
        let mut kept: Vec<usize> = by_area[..keep].to_vec();
        kept.sort_unstable();
        let subset: Vec<&FillFeature> = kept.iter().map(|&i| &fill[i]).collect();
        let mut mvt = encode(&subset);

        #[cfg(test)]
        coverage_fill::cost_probe::update(|c| {
            c.fill_passes += 1;
        });
        #[cfg(test)]
        let gzip_before = mvt.gzip.as_ref().map(|g| g.as_ptr());
        let fit = tile_fit(&mut mvt, opts).unwrap_or(f64::INFINITY);
        #[cfg(test)]
        note_gzip(gzip_before, &mvt);
        if fit >= 1.0 {
            let (polygons, vertices) = subset
                .iter()
                .fold((0, 0), |(p, v), f| (p + f.polygons, v + f.vertices));
            tally.shed += total - polygons;
            tally.polygons = polygons;
            tally.vertices = vertices;
            return TileMvt {
                oversized: fitted.oversized,
                valve: ValveTally {
                    passes: fitted.valve.passes + pass,
                    ..fitted.valve
                },
                ..mvt
            };
        }
        // Shed the fill's share of the overrun, with a margin: the kept
        // fill's bytes, less what the whole tile is over by.
        let fill_bytes = mvt.data.len().saturating_sub(fitted.data.len()).max(1) as f64;
        let over = mvt.data.len() as f64 * (1.0 - fit);
        let share = ((fill_bytes - over) / fill_bytes * 0.9).max(0.0);
        let budget = subset.iter().map(|f| f.cmds.len()).sum::<usize>() as f64 * share;
        let mut used = 0usize;
        let next = by_area[..keep]
            .iter()
            .take_while(|&&i| {
                used += fill[i].cmds.len();
                used as f64 <= budget
            })
            .count();
        keep = next.min(keep - 1);
        if keep == 0 {
            break;
        }
    }
    // None of the fill fit: the tile goes without, but its encodes count.
    fitted.valve.passes += tried;
    no_fill(tally, fitted)
}

/// Whether the size valve can merge anything: [`SizeValve::Merge`] with a
/// size limit in force. Otherwise no gap is ever compared, and the read
/// skips measuring them (#745).
pub(super) fn valve_merges(opts: &ExportOptions) -> bool {
    opts.size_valve == SizeValve::Merge && opts.has_tile_size_cap()
}

/// Whether every member has at most one coordinate: a tile of points (or of
/// polygons collapsed to centroid points, the #259 dot-fill recipe).
pub(super) fn is_point_like<M: TileFeature>(members: &[M]) -> bool {
    members.iter().all(|m| m.geometry().coords_count() <= 1)
}

/// The share of a tile that fits its size limits: the smallest
/// `limit / size` over the limits in force, so `>= 1.0` fits (infinite with
/// no limit). Compresses the tile only when a compressed limit is set, and
/// keeps the bytes for the writer. Reuses gzip only while the raw data is
/// unchanged; `TileData` invalidates it on every mutable raw-buffer access.
/// Both currently active limits are recomputed even on a cache hit.
pub(super) fn tile_fit(mvt: &mut TileMvt, opts: &ExportOptions) -> Result<f64, ExportError> {
    check_mlt(mvt)?;
    let mut fit = f64::INFINITY;
    if let Some(limit) = opts.tile_size_limit.filter(|&l| l > 0) {
        fit = fit.min(limit as f64 / mvt.data.len() as f64);
    }
    if let Some(limit) = opts.compressed_tile_size_limit.filter(|&l| l > 0) {
        let gz = mvt.data.gzipped(&mvt.gzip_cache)?;
        fit = fit.min(limit as f64 / gz.len() as f64);
    }
    Ok(fit)
}

/// The export's failure for a tile whose MVT did not convert to MLT.
pub(super) fn check_mlt(mvt: &TileMvt) -> Result<(), ExportError> {
    match &mvt.mlt_error {
        Some(message) => Err(ExportError::MltConversion {
            message: message.clone(),
        }),
        None => Ok(()),
    }
}

/// Encodes the drop valve may take before it gives up and writes the tile
/// over its limit. Each pass keeps the share of the last that fit, so a
/// tile needing more than a few is pathological.
pub(crate) const MAX_DROP_PASSES: u32 = 8;

/// How many of `keep` features the drop loops keep on cut `cut` (0-based)
/// of a tile at `fit`: the share that fits, with tippecanoe's 0.8 margin
/// from the second cut on. Without it a tile just over its cap shed a
/// sliver per pass and ran out of passes still over (#873).
pub(super) fn drop_keep(keep: usize, fit: f64, cut: u32) -> usize {
    // DIVERGENCE FROM TIPPECANOE: tile.cpp applies its 0.8 margin on the
    // first cut too. We keep the first cut as before, so a tile that fit
    // after one cut is byte-identical.
    (keep as f64 * fit * if cut == 0 { 1.0 } else { 0.8 }).floor() as usize
}

/// One grid unit per pixel on a 512-pixel tile before losing whole features.
pub(super) const MIN_VALVE_EXTENT: u32 = 512;

/// The drop valve: while the tile is over its limit, keep a proportional
/// count of members and re-encode. `select_kept_members` decides which
/// survive.
///
/// Before #745 this was a single pass that never re-measured, and a tile
/// whose survivors still encoded too big was written over its limit (45 of
/// 1,286 tiles on the FTW Moldova benchmark). The first pass is unchanged,
/// so a tile that fit after one pass is byte-identical to before.
pub(super) fn drop_to_fit(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &TileEncoding<'_>,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    let layers = opts.layers;
    let mut full = build_tile_layers(gzip_cache, layers, members, tb, opts, &[], diagnostics);
    let mut fit = tile_fit(&mut full, opts).unwrap_or(f64::INFINITY);
    if fit >= 1.0 || members.len() <= 1 {
        // One encode, as the merge valve counts a tile that fits at once.
        let fitted = TileMvt {
            valve: ValveTally {
                passes: 1,
                over_limit: usize::from(fit < 1.0),
                ..ValveTally::default()
            },
            ..full
        };
        return with_fill(fitted, opts, fill, fill_tally, |f| {
            build_tile_layers(gzip_cache, layers, members, tb, opts, f, diagnostics)
        });
    }
    // Later passes need only the original size for the warning.
    let full_bytes = full.data.len();
    drop(full);
    let mut keep = members.len();
    let mut passes = 1u32;
    loop {
        passes += 1;
        keep = drop_keep(keep, fit, passes - 2).clamp(1, keep.saturating_sub(1).max(1));
        let kept = shed_to_fit(members, keep, opts);
        let mut shed = build_tile_layers(gzip_cache, layers, &kept, tb, opts, &[], diagnostics);
        fit = tile_fit(&mut shed, opts).unwrap_or(f64::INFINITY);
        if fit >= 1.0 || keep <= 1 || passes >= MAX_DROP_PASSES {
            log::debug!(target: LOG_TARGET,
                "oversized tile ({} bytes): dropped {} of {} features ({passes} encodes){}",
                full_bytes,
                members.len() - keep,
                members.len(),
                if fit < 1.0 {
                    "; still over the limit"
                } else {
                    ""
                },
            );
            let fitted = TileMvt {
                oversized: true,
                valve: ValveTally {
                    dropped: members.len() - keep,
                    passes,
                    over_limit: usize::from(fit < 1.0),
                    ..ValveTally::default()
                },
                ..shed
            };
            return with_fill(fitted, opts, fill, fill_tally, |f| {
                build_tile_layers(gzip_cache, layers, &kept, tb, opts, f, diagnostics)
            });
        }
    }
}

/// How many more passes a stalling tile may need, at the rate its last pass
/// shrank it, before the merge valve stops raising the threshold step by
/// step and merges everything (#758).
pub(super) const MAX_PROJECTED_PASSES: f64 = 4.0;

/// Has the merge valve stalled? From the fit before and after a pass: the
/// tile did not shrink, or shrinking at that rate it would need more than
/// [`MAX_PROJECTED_PASSES`] further passes to fit. Merging features that do
/// not touch saves only their tags, so on detailed, scattered polygons each
/// pass shrinks the tile by a few percent.
pub(super) fn stalled(prev_fit: f64, fit: f64) -> bool {
    let rate = fit / prev_fit;
    rate <= 1.0 || (1.0 / fit).ln() / rate.ln() > MAX_PROJECTED_PASSES
}

/// The merge valve (#745): merge members whose gap is below the threshold
/// into the member kept before them, re-encode, and raise the threshold
/// until the tile fits. See [`coalesce_valve`]. A tile that stops shrinking
/// ([`stalled`]) for two passes in a row merges everything it can at once
/// (#758).
///
/// A tile under its limit is encoded exactly as [`drop_to_fit`] would. When
/// the threshold cannot rise any further and the tile is still over, the
/// unmerged features are dropped, smallest first. Then halve the extent to
/// 512 before dropping merged features, smallest first. Only a lone feature
/// that still exceeds the cap can be written over it, with a warning.
pub(super) fn merge_to_fit(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &TileEncoding<'_>,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    let layers = opts.layers;
    // The merge walks the file's spatial order; `--feature-order` sorted the
    // tile by a column instead.
    let mut ordered: Vec<&Member> = members.iter().collect();
    if !matches!(opts.feature_order, FeatureOrder::Input) {
        ordered.sort_unstable_by_key(|m| m.seq);
    }
    // Every pass keeps targets from `ordered`. Restricting this global
    // stable order to those targets gives the same two stable sorts, without
    // repeating the comparisons after each merge or drop.
    let mut draw_indices: Vec<usize> = (0..ordered.len()).collect();
    #[cfg(test)]
    crate::valve_counter::draw_sort(draw_indices.len());
    draw_indices.sort_by_key(|&i| ordered[i].seq);
    if let FeatureOrder::Column { name, descending } = &opts.feature_order {
        #[cfg(test)]
        crate::valve_counter::draw_sort(draw_indices.len());
        draw_indices
            .sort_by(|&a, &b| compare_by_feature_order(ordered[a], ordered[b], name, *descending));
    }
    let mut draw_rank = vec![0; ordered.len()];
    for (rank, i) in draw_indices.into_iter().enumerate() {
        draw_rank[i] = rank;
    }
    // Store positions rather than references: each pass owns new merged
    // features, but the tile-sized scratch buffer survives every pass.
    let mut ranked = vec![None; ordered.len()];
    let mut draw_order = |feats: &mut Vec<&coalesce_valve::Merged<'_>>| {
        for (i, m) in feats.iter().enumerate() {
            ranked[draw_rank[m.members[0]]] = Some(i);
        }
        let mut dest = 0;
        for rank in 0..ranked.len() {
            let Some(src) = ranked[rank].take() else {
                continue;
            };
            if src != dest {
                // The feature displaced from `dest` now lives at `src`.
                ranked[draw_rank[feats[dest].members[0]]] = Some(src);
                feats.swap(dest, src);
            }
            dest += 1;
        }
    };
    // #758: from the second pass on, every member's polygons are projected,
    // quantized and cleaned once, so a pass only unions, and only the
    // components that changed since the pass before. A tile that fits at
    // once never builds this. Below the pair cap, interactions are cached;
    // above it, candidates are swept on demand. Parts are tested only when
    // a pass puts both in one feature: on NZ Parcels under a 2 MB gzip cap,
    // where most oversized tiles fit on their second pass, testing every
    // touching pair up front cost 15% of the export.
    // Built once, on the first pass that merges; `Some(None)` only when the
    // part count cannot fit the cache's u32 indices.
    let mut quantized: Option<Option<crate::mvt::QuantizedPolygons>> = None;
    let mut build = |feats: &[&coalesce_valve::Merged<'_>], pass: u32, fill: &[&FillFeature]| {
        // Pass 1 merges nothing, and a pass that still merges nothing (a
        // tile with no mergeable gaps, on its way to the drop fallback) has
        // nothing to reuse.
        if pass == 1 || feats.iter().all(|m| m.members.len() < 2) {
            return build_tile_layers(gzip_cache, layers, feats, tb, opts, fill, diagnostics);
        }
        let cached = quantized.get_or_insert_with(|| {
            crate::mvt::QuantizedPolygons::new(
                ordered.iter().map(|m| (&*m.geom, u64::from(m.band))),
                tb,
                opts.extent,
            )
        });
        let Some(q) = cached.as_mut() else {
            return build_tile_layers(gzip_cache, layers, feats, tb, opts, fill, diagnostics);
        };
        q.next_pass();
        let q = &*q;
        build_tile_layers_with(gzip_cache, layers, feats, opts, fill, |m| {
            q.encode(&m.members, diagnostics).unwrap_or_else(|| {
                EncodedFeatureGeometry::encode(m.geometry(), tb, opts.extent, diagnostics)
            })
        })
    };
    let mut gaps: Option<Vec<u64>> = None;
    let mut mingap = 0u64;
    let mut keep_fraction = 1.0_f64;
    let mut prev_fit: Option<f64> = None;
    let mut stalls = 0u32;
    let mut passes = 0u32;
    loop {
        passes += 1;
        let merged = coalesce_valve::coalesce_members(&ordered, mingap);
        let absorbed: usize = merged.iter().map(|m| m.absorbed).sum();
        let mut feats: Vec<&coalesce_valve::Merged<'_>> = merged.iter().collect();
        draw_order(&mut feats);
        let mut mvt = build(&feats, passes, &[]);
        let fit = tile_fit(&mut mvt, opts).unwrap_or(f64::INFINITY);
        let tally = ValveTally {
            merged: absorbed,
            dropped: 0,
            passes,
            mingap,
            over_limit: usize::from(fit < 1.0),
            ..ValveTally::default()
        };
        if fit >= 1.0 {
            let fitted = TileMvt {
                oversized: passes > 1,
                valve: tally,
                ..mvt
            };
            let mut fitted =
                with_fill(fitted, opts, fill, fill_tally, |f| build(&feats, passes, f));
            if absorbed > 0 && opts.feature_id.is_some() {
                account_merged_sources(&merged, &feats, &ordered, &mut fitted);
            }
            return fitted;
        }
        // tippecanoe's `mingap_fraction`: aim for the share of features
        // that would fit, with a 20% margin, never keeping more than 80%.
        keep_fraction = (keep_fraction * fit * 0.8).min(0.8);
        // #758: a tile that shrinks too slowly to fit within a few more
        // passes, twice in a row, merges everything it can in one step. One
        // slow pass is not enough: gzip barely rewards a pass that merges
        // only a few features, so a tile just over a compressed cap can look
        // stalled once and still fit a pass later.
        // DIVERGENCE FROM TIPPECANOE: tippecanoe keeps raising the threshold
        // one quantile at a time until it fits or runs out of gaps.
        stalls = if prev_fit.is_some_and(|p| stalled(p, fit)) {
            stalls + 1
        } else {
            0
        };
        if stalls >= 2 {
            keep_fraction = 0.0;
        }
        prev_fit = Some(fit);
        let g = gaps.get_or_insert_with(|| coalesce_valve::mergeable_gaps(&ordered));
        if let Some(next) = coalesce_valve::next_mingap(g, keep_fraction, mingap) {
            mingap = next;
            continue;
        }
        // Merged as far as the gaps allow and still over.
        // DIVERGENCE FROM TIPPECANOE: tippecanoe exits ("Can't increase
        // feature gap threshold further"); we finish the tile.
        let (whole, single): (Vec<_>, Vec<_>) = feats.iter().copied().partition(|m| m.absorbed > 0);
        let mut kept = feats;
        let mut dropped = 0;
        let mut out = mvt;
        let mut fit = fit;
        if !single.is_empty() {
            // Drop unmerged features until it fits: fewest vertices first,
            // points thinned evenly (`select_kept_members`).
            // With merged features in the tile, every single may go before
            // merged coverage is touched; alone, the largest single stays.
            let min_keep = usize::from(whole.is_empty());
            let mut keep = single.len();
            for cut in 0..MAX_DROP_PASSES {
                // Nothing left to drop: go straight to the backstop.
                if keep <= min_keep {
                    break;
                }
                keep = drop_keep(keep, fit, cut).min(keep - 1).max(min_keep);
                kept = whole.clone();
                if keep > 0 {
                    kept.extend(select_kept_members(&single, keep).into_iter().copied());
                }
                draw_order(&mut kept);
                passes += 1;
                // The previous bytes cannot be returned after dropping more
                // features. Release them before building their replacement.
                drop(out);
                out = build(&kept, passes, &[]);
                fit = tile_fit(&mut out, opts).unwrap_or(f64::INFINITY);
                if fit >= 1.0 || keep == 0 {
                    break;
                }
            }
            // The cuts ran out with singles left beside merged features: the
            // rest go too, before the extent drops or a merge is touched.
            if fit < 1.0 && !whole.is_empty() && kept.len() > whole.len() {
                kept = whole.clone();
                draw_order(&mut kept);
                passes += 1;
                drop(out);
                out = build(&kept, passes, &[]);
                fit = tile_fit(&mut out, opts).unwrap_or(f64::INFINITY);
            }
            dropped = single.len() - (kept.len() - whole.len());
        }
        let mut tally = ValveTally {
            dropped,
            passes,
            over_limit: usize::from(fit < 1.0),
            ..tally
        };
        if fit < 1.0 {
            out = reduce_merged_to_fit(
                gzip_cache,
                &mut kept,
                tb,
                (opts, opts.layers),
                diagnostics,
                out,
                &mut tally,
            );
        }
        log::debug!(target: LOG_TARGET,
            "oversized tile: merged {absorbed} of {} features and dropped {} sources \
             ({} merged features carrying {} sources){}",
            members.len(),
            tally.dropped,
            tally.dropped_merged,
            tally.dropped_merged_sources,
            if tally.over_limit > 0 {
                "; single remaining feature still over the limit, written as is"
            } else {
                ""
            },
        );
        let fitted = TileMvt {
            oversized: true,
            valve: tally,
            ..out
        };
        let mut fitted = with_fill(fitted, opts, fill, fill_tally, |f| {
            build(&kept, tally.passes, f)
        });
        if opts.feature_id.is_some() {
            account_merged_sources(&merged, &kept, &ordered, &mut fitted);
        }
        return fitted;
    }
}

/// Re-encode the exhausted merge valve's survivors on coarser grids, then
/// drop whole features at the floor. Only a lone feature may stay over.
pub(super) fn reduce_merged_to_fit(
    gzip_cache: &Arc<TileGzipCache>,
    kept: &mut Vec<&coalesce_valve::Merged<'_>>,
    tb: &TileBounds,
    (opts, layers): (&ExportOptions, &[(u16, String)]),
    diagnostics: &crate::diagnostics::Diagnostics,
    mut out: TileMvt,
    tally: &mut ValveTally,
) -> TileMvt {
    // DIVERGENCE FROM TIPPECANOE: tile.cpp:2946 exits when coalescing
    // exhausts gaps. Its detail loop (3031) runs only without an as-needed
    // strategy and reaches 128. We reduce detail after merging, to 512,
    // then drop whole features. A lone feature still over the cap at that
    // floor is kept with a warning, without splitting it into its sources.
    let mut reduced = opts.clone();
    let mut fit = tile_fit(&mut out, opts).unwrap_or(f64::INFINITY);
    // A coarser grid only moves points; a point tile is dropped as the drop
    // valve drops it.
    let points_only = kept
        .iter()
        .all(|m| matches!(m.geometry(), Geometry::Point(_) | Geometry::MultiPoint(_)));
    while fit < 1.0 && reduced.extent > MIN_VALVE_EXTENT && !points_only {
        reduced.extent = (reduced.extent >> 1).max(MIN_VALVE_EXTENT);
        tally.passes += 1;
        drop(out);
        // Plain encode quantizes, unions and cleans geometry on this grid.
        // Never reuse the original grid's QuantizedPolygons or fill.
        out = build_tile_layers(gzip_cache, layers, kept, tb, &reduced, &[], diagnostics);
        fit = tile_fit(&mut out, &reduced).unwrap_or(f64::INFINITY);
    }
    while fit < 1.0 && kept.len() > 1 {
        let keep = drop_keep(kept.len(), fit, 1).clamp(1, kept.len() - 1);
        let selected: HashSet<_> = select_kept_members(kept, keep)
            .iter()
            .map(|m| m.members[0])
            .collect();
        // Restrict the existing draw_order to its survivors, including
        // feature-order columns, on every encode used to measure the cap.
        kept.retain(|m| {
            if selected.contains(&m.members[0]) {
                return true;
            }
            tally.dropped += m.members.len();
            // Its absorbed sources are lost, not merged: count each once.
            tally.merged -= m.absorbed;
            if m.absorbed > 0 {
                tally.dropped_merged += 1;
                tally.dropped_merged_sources += m.members.len();
            }
            false
        });
        tally.passes += 1;
        drop(out);
        out = build_tile_layers(gzip_cache, layers, kept, tb, &reduced, &[], diagnostics);
        fit = tile_fit(&mut out, &reduced).unwrap_or(f64::INFINITY);
    }
    tally.over_limit = usize::from(fit < 1.0);
    tally.over_limit_merged = fit < 1.0 && kept.len() == 1 && kept[0].absorbed > 0;
    tally.reduced_extent = (reduced.extent < opts.extent).then_some(reduced.extent);
    if let Some(extent) = tally.reduced_extent {
        log::debug!(target: LOG_TARGET, "tile bounds {tb:?} written at extent {extent}");
    }
    out
}

/// The merge valve's per-source loss owner, after the final encode (including
/// fill). The encoder tracks targets; propagate their encode losses to absorbed
/// sources, otherwise classify kept merges as merged and dropped features'
/// sources as valve-dropped, whether or not the backstop ran.
pub(super) fn account_merged_sources(
    merged: &[coalesce_valve::Merged<'_>],
    kept: &[&coalesce_valve::Merged<'_>],
    ordered: &[&Member],
    out: &mut TileMvt,
) {
    use crate::diagnostics::SourceLossCause;
    let retained: HashSet<_> = kept.iter().map(|m| m.members[0]).collect();
    let losses: HashMap<_, _> = out.source_losses.iter().copied().collect();
    for m in merged {
        let retained = retained.contains(&m.members[0]);
        let cause = if retained {
            losses
                .get(&m.target.seq)
                .copied()
                .unwrap_or(SourceLossCause::Merged)
        } else {
            SourceLossCause::ValveDropped
        };
        for &i in &m.members[usize::from(retained)..] {
            out.source_losses.push((ordered[i].seq, cause));
        }
    }
}

/// The survivors of the oversized-tile valve, **in the order they are drawn**.
///
/// Two separate decisions, deliberately kept apart: [`select_kept_members`]
/// decides *which* features survive, ranking sized geometry by vertex count;
/// this then restores the caller's draw order over them. Emitting the
/// selection ranking as the draw order would silently override a pinned
/// `--feature-order` — and only on oversized tiles, i.e. exactly the dense
/// ones a caller pins an order for. The default [`FeatureOrder::Input`] keeps
/// the vertex-count order, which is the pre-#280 behaviour and byte-identical
/// to it.
pub(super) fn shed_to_fit<'a>(
    members: &'a [Member],
    keep: usize,
    opts: &ExportOptions,
) -> Vec<&'a Member> {
    let mut kept = select_kept_members(members, keep);
    if let FeatureOrder::Column { name, descending } = &opts.feature_order {
        kept.sort_by(|a, b| compare_by_feature_order(a, b, name, *descending));
    }
    kept
}

/// Choose which `keep` of `members` survive the oversized-tile drop pass.
///
/// The single-pass valve has exactly one per-feature ranking signal — geometry
/// vertex count — and that signal is degenerate for point tiles, so there are
/// two regimes:
///
/// * **Sized geometries** (lines / polygons): keep the `keep` largest by
///   coordinate count. The biggest features carry the tile's visual signal;
///   this is the pre-#280 behaviour and is byte-identical to it.
/// * **Point-dominated tiles** (every member ≤ 1 coordinate — the #259 dot-fill
///   recipe collapses dense polygons to centroid points): vertex count carries
///   no signal, so a size-ranked prefix would just keep whichever points happen
///   to sort first and clump them in one corner. Instead keep a uniform stride
///   across member order. Overview rows are in the source file's order, which
///   for gpio-optimized input is Hilbert order, so a tile's members arrive in
///   space-filling-curve order; an even stride then spreads the survivors
///   across the tile — the same spatially uniform thinning tippecanoe
///   achieves by dropping every Nth feature in sequence.
///
/// `keep` is clamped to `1..=members.len()`.
pub(super) fn select_kept_members<M: TileFeature>(members: &[M], keep: usize) -> Vec<&M> {
    select_kept_by_size(members, keep, is_point_like(members), |m| {
        m.geometry().coords_count()
    })
}

/// Shared drop ranking for overview members and decoded joined-tile features.
/// The latter have geometry commands rather than source coordinates.
pub(crate) fn select_kept_by_size<M>(
    members: &[M],
    keep: usize,
    point_like: bool,
    size: impl Fn(&M) -> usize,
) -> Vec<&M> {
    let n = members.len();
    let keep = keep.clamp(1, n);
    if point_like {
        // Evenly spaced indices 0, n/keep, 2n/keep, … — strictly increasing and
        // in-bounds because keep ≤ n.
        (0..keep).map(|i| &members[i * n / keep]).collect()
    } else {
        let mut ranked: Vec<&M> = members.iter().collect();
        ranked.sort_by_key(|m| std::cmp::Reverse(size(m)));
        ranked.truncate(keep);
        ranked
    }
}

#[cfg(test)]
mod diagnostic_row_alignment {
    use super::*;

    /// A kept merge's absorbed rows have the same cause whether dropping
    /// singles fits the tile or the extent backstop must finish it.
    #[test]
    fn merge_valve_source_causes_do_not_depend_on_extent_backstop() {
        use crate::diagnostics::{Diagnostics, SourceLossCause};

        let members: Vec<_> = [
            Geometry::LineString(geo::LineString::from(vec![(-10., 0.), (10., 0.)])),
            Geometry::LineString(geo::LineString::from(vec![(-10., 0.), (10., 0.)])),
            Geometry::Point(geo::Point::new(0., 0.)),
            Geometry::Point(geo::Point::new(0., 0.)),
        ]
        .into_iter()
        .enumerate()
        .map(|(row, geom)| Member {
            key: 0,
            seq: row as u64,
            geom: geom.into(),
            props: SharedProps::new(vec![]),
            id: Some(100 + row as u64),
            gap: if row == 0 { GAP_NONE } else { 1 },
            band: 0,
        })
        .collect();
        let mut opts = ExportOptions {
            feature_id: Some("id".into()),
            size_valve: SizeValve::Merge,
            compressed_tile_size_limit: None,
            ..Default::default()
        };
        let cache = test_gzip_cache();
        let tb = TileCoord::new(0, 0, 0).bounds();
        let ordered: Vec<_> = members.iter().collect();
        let merged = coalesce_valve::coalesce_members(&ordered, 2);
        // Exactly enough room for the merged lines and one unmerged point.
        let singles_limit = build_mvt(
            &cache,
            &merged[..2],
            &tb,
            &opts,
            &[],
            &Diagnostics::default(),
        )
        .data
        .len();
        let merged_limit = build_mvt(&cache, &merged, &tb, &opts, &[], &Diagnostics::default())
            .data
            .len();
        for (limit, dropped, backstop) in [
            (merged_limit, 0, false),
            (1, 2, true),
            (singles_limit, 1, false),
        ] {
            opts.tile_size_limit = Some(limit);
            let diagnostics = Diagnostics::default();
            let mvt = encode_tile(
                &cache,
                &members,
                &tb,
                &opts,
                &[],
                &mut FillTally::default(),
                &diagnostics,
            );
            assert_eq!(mvt.valve.reduced_extent.is_some(), backstop);
            assert_eq!(mvt.valve.merged, 1);
            assert_eq!(mvt.valve.dropped, dropped);
            assert!(mvt.source_ids.contains(&0), "merged feature is kept");
            account_sources(&members, &mvt, 0, &opts, &diagnostics).unwrap();
            let losses = diagnostics.source_losses().unwrap();
            let actual: Vec<_> = losses
                .iter()
                .map(|loss| {
                    let loss = loss.unwrap();
                    assert_eq!(loss.feature_id, Some(100 + loss.id));
                    (loss.id, loss.cause)
                })
                .collect();
            let mut expected = vec![(1, SourceLossCause::Merged)];
            if dropped == 2 {
                expected.push((2, SourceLossCause::ValveDropped));
            }
            if dropped > 0 {
                expected.push((3, SourceLossCause::ValveDropped));
            }
            assert_eq!(actual, expected, "backstop={backstop}");
            assert_eq!(
                mvt.source_losses, expected,
                "valve records each source's cause"
            );
        }
    }

    #[test]
    fn mutation_regression_source_loss_valve_causes() {
        use crate::diagnostics::{Diagnostics, SourceLossCause};

        let line = || Geometry::LineString(geo::LineString::from(vec![(-10., 0.), (10., 0.)]));
        let point = || Geometry::Point(geo::Point::new(0., 0.));
        let empty = || Geometry::GeometryCollection(geo::GeometryCollection::default());
        // With no valve loss, every lost row has an explicit encode cause.
        // That overrides the tile fallback, making `merged >= 0` equivalent
        // to `merged > 0` for the recorded losses in the zero/zero case.
        for (geoms, valve, limit, dropped, merged, expected) in [
            (
                vec![point(), empty()],
                SizeValve::Drop,
                None,
                0,
                0,
                vec![(1, SourceLossCause::EncodeDropped)],
            ),
            (
                vec![line(), line()],
                SizeValve::Merge,
                Some(1),
                0,
                1,
                vec![(1, SourceLossCause::Merged)],
            ),
            (
                vec![point(), point()],
                SizeValve::Drop,
                Some(1),
                1,
                0,
                vec![(1, SourceLossCause::ValveDropped)],
            ),
            (
                vec![line(), line(), point(), point()],
                SizeValve::Merge,
                Some(1),
                2,
                1,
                vec![
                    (1, SourceLossCause::Merged),
                    (2, SourceLossCause::ValveDropped),
                    (3, SourceLossCause::ValveDropped),
                ],
            ),
        ] {
            let members: Vec<_> = geoms
                .into_iter()
                .enumerate()
                .map(|(row, geom)| Member {
                    key: 0,
                    seq: row as u64,
                    geom: geom.into(),
                    props: SharedProps::new(vec![]),
                    id: Some(100 + row as u64),
                    gap: if row == 0 { GAP_NONE } else { 1 },
                    band: 0,
                })
                .collect();
            let opts = ExportOptions {
                feature_id: Some("id".into()),
                size_valve: valve,
                tile_size_limit: limit,
                compressed_tile_size_limit: None,
                ..Default::default()
            };
            let expected_features = members.len() - expected.len();
            let cache = test_gzip_cache();
            let mvt = encode_tile(
                &cache,
                &members,
                &TileCoord::new(0, 0, 0).bounds(),
                &opts,
                &[],
                &mut FillTally::default(),
                &Diagnostics::default(),
            );
            assert_eq!(
                (mvt.valve.dropped, mvt.valve.merged),
                (dropped, merged),
                "valve={:?}, features={}, bytes={}",
                mvt.valve,
                mvt.features,
                mvt.data.len()
            );
            let diagnostics = Diagnostics::default();
            let tiles = encode_members(
                &cache,
                members,
                0,
                &opts,
                None,
                &ExportTimers::default(),
                &diagnostics,
            )
            .unwrap();
            assert_eq!(tiles.len(), 1);
            assert_eq!(tiles[0].feature_count, expected_features);
            let losses = diagnostics.source_losses().unwrap();
            let actual: Vec<_> = losses
                .iter()
                .map(|loss| {
                    let loss = loss.unwrap();
                    assert_eq!(loss.feature_id, Some(100 + loss.id));
                    (loss.id, loss.cause)
                })
                .collect();
            assert_eq!(actual, expected, "dropped={dropped}, merged={merged}");
        }
    }

    #[test]
    fn duplicate_feature_ids_do_not_cancel_another_source_rows_loss() {
        let diagnostics = crate::diagnostics::Diagnostics::default();
        let geoms = vec![
            Geometry::Point(geo::Point::new(1.0, 1.0)),
            Geometry::Polygon(geo::Polygon::new(
                geo::LineString::from(vec![
                    (0.0, 0.0),
                    (0.000001, 0.0),
                    (0.000001, 0.000001),
                    (0.0, 0.0),
                ]),
                vec![],
            )),
        ];
        let members: Vec<Member> = geoms
            .into_iter()
            .enumerate()
            .map(|(row, geom)| {
                diagnostics
                    .source_member_loss(
                        row as u64,
                        7,
                        0,
                        crate::diagnostics::SourceLossCause::ClipDropped,
                    )
                    .unwrap();
                Member {
                    key: 0,
                    seq: row as u64,
                    geom: geom.into(),
                    id: Some(7),
                    props: SharedProps::new(vec![]),
                    gap: GAP_NONE,
                    band: 0,
                }
            })
            .collect();
        assert_eq!(members[0].source_row(), 0);
        let tiles = encode_members(
            &test_gzip_cache(),
            members,
            0,
            &ExportOptions {
                feature_id: Some("id".into()),
                tile_size_limit: None,
                compressed_tile_size_limit: None,
                ..Default::default()
            },
            None,
            &ExportTimers::default(),
            &diagnostics,
        )
        .unwrap();
        assert_eq!(tiles.len(), 1);
        assert_eq!(tiles[0].feature_count, 1);
        let losses = diagnostics.source_losses().unwrap();
        assert_eq!(losses.len(), 1);
        assert_eq!(losses.iter().next().unwrap().unwrap().id, 1);
        assert_eq!(losses.iter().next().unwrap().unwrap().feature_id, Some(7));
        assert_eq!(
            losses.iter().next().unwrap().unwrap().cause,
            crate::diagnostics::SourceLossCause::Quantized
        );
    }

    /// A tile whose own features all quantize away is not written, so a
    /// #750 fill drawn for it must not count its host as emitted: an
    /// emission cancels the host's recorded loss. `features` excludes the fill, while
    /// `source_ids` includes it, which is what the `features > 0` guard in
    /// `encode_members` separates.
    #[test]
    fn mutation_regression_fill_of_an_empty_tile_does_not_emit_its_host() {
        use crate::diagnostics::{Diagnostics, SourceLossCause};

        let triangle = Geometry::Polygon(geo::Polygon::new(
            geo::LineString::from(vec![
                (0.0, 0.0),
                (0.000001, 0.0),
                (0.000001, 0.000001),
                (0.0, 0.0),
            ]),
            vec![],
        ));
        let members = vec![Member {
            key: 0,
            seq: 0,
            geom: triangle.into(),
            props: SharedProps::new(vec![]),
            id: Some(7),
            gap: GAP_NONE,
            band: 0,
        }];
        let fills = coverage_fill::tests::test_fill_map([(
            0,
            0,
            vec![vec![vec![
                (0, 0),
                (1000, 0),
                (1000, 1000),
                (0, 1000),
                (0, 0),
            ]]],
        )]);
        let opts = ExportOptions {
            feature_id: Some("id".into()),
            tile_size_limit: None,
            compressed_tile_size_limit: None,
            ..Default::default()
        };
        let diagnostics = Diagnostics::default();
        // The host was already lost upstream; an emission would cancel that.
        diagnostics
            .source_member_loss(0, 7, 0, SourceLossCause::ClipDropped)
            .unwrap();
        let tiles = encode_members(
            &test_gzip_cache(),
            members,
            0,
            &opts,
            Some(&fills),
            &ExportTimers::default(),
            &diagnostics,
        )
        .unwrap();
        assert!(tiles.iter().all(|t| t.feature_count == 0), "no own feature");
        let losses = diagnostics.source_losses().unwrap();
        let causes: Vec<_> = losses.iter().map(|l| l.unwrap().cause).collect();
        assert_eq!(causes, vec![SourceLossCause::ClipDropped]);
    }

    #[test]
    fn export_decode_keeps_null_geometry_rows_aligned_with_feature_ids() {
        let input = tempfile::NamedTempFile::new().unwrap();
        crate::overview::testutil::write_input(
            input.path(),
            &[None, Some(Geometry::Point(geo::Point::new(12.0, 34.0)))],
            true,
            None,
        );
        let source = crate::input::InputSource::from_path(input.path()).unwrap();
        let batch = source
            .open()
            .unwrap()
            .build()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let schema = batch.schema();
        let diagnostics = crate::diagnostics::Diagnostics::default();
        let geoms = decode_batch_geometries(
            &batch,
            &schema,
            geometry_index(&schema).unwrap(),
            Crs::Epsg4326,
            &ExportTimers::default(),
            &diagnostics,
        )
        .unwrap();
        assert_eq!(geoms.len(), 2, "null rows must retain their source ID slot");
        assert_eq!(geoms[0].coords_count(), 0);
        assert_eq!(geoms[1], Geometry::Point(geo::Point::new(12.0, 34.0)));
    }
}
