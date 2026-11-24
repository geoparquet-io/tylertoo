use super::*;

/// Within-tile feature order (#361).
///
/// MVT does not define draw order, but renderers paint features in the order
/// the tile lists them, so this *is* the paint order for any style that does
/// not override it (`fill-sort-key` and friends). Neither tylertoo nor
/// tippecanoe specified one before, which made a tiler swap a silent visual
/// change; naming it makes it something a caller can depend on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum FeatureOrder {
    /// Input row order — the overview file's row order, which is the source
    /// file's row order restricted to the rows the level kept. This is what
    /// tylertoo has always emitted and stays the default.
    #[default]
    Input,
    /// Ascending (or descending) by an MVT property, ties broken by input
    /// order so the result is fully determined.
    Column {
        /// Property name as it appears in the tile.
        name: String,
        /// Sort high → low instead of low → high.
        descending: bool,
    },
}

/// The spelling [`FeatureOrder::from_str`](std::str::FromStr) parses:
/// `input`, or `COLUMN` / `COLUMN:desc`.
impl std::fmt::Display for FeatureOrder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Input => f.write_str("input"),
            Self::Column {
                name,
                descending: false,
            } => f.write_str(name),
            Self::Column {
                name,
                descending: true,
            } => write!(f, "{name}:desc"),
        }
    }
}

impl std::str::FromStr for FeatureOrder {
    type Err = String;

    /// `input` | `<column>` | `<column>:asc` | `<column>:desc`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "input" {
            return Ok(Self::Input);
        }
        let (name, descending) = match s.rsplit_once(':') {
            Some((n, "asc")) => (n, false),
            Some((n, "desc")) => (n, true),
            // A trailing `:something-else` is a mistyped direction far more
            // often than a column whose name contains a colon; say so rather
            // than silently sorting by a column that does not exist.
            Some((_, other)) => {
                return Err(format!(
                    "unknown sort direction {other:?}: expected `asc` or `desc` \
                     (as in `--feature-order level:desc`)"
                ))
            }
            None => (s, false),
        };
        if name.is_empty() {
            return Err("expected `input` or a column name, e.g. `level:desc`".to_string());
        }
        Ok(Self::Column {
            name: name.to_string(),
            descending,
        })
    }
}

/// A member's sort key for [`FeatureOrder::Column`].
///
/// Numeric property variants collapse to one `f64` class so a column read as
/// `Int` in one batch and `Double` in another does not split into two
/// incomparable groups. `Missing` sorts first (painted underneath): a feature
/// the style cannot rank should not be promoted above ones it can.
///
/// The variant order *is* the class order — `Missing` < `Number` < `Text` —
/// and [`OrderKey::cmp`] is a genuine total order, which the caller's
/// `sort_by` requires. Deferring to `f64`'s *partial* order and calling an
/// incomparable pair equal would not be one: with `1.0 == NaN`, `NaN == 2.0`
/// and `1.0 < 2.0`, equality is not transitive, and the standard library
/// detects that and panics. So a NaN — how plenty of sources spell nodata in
/// a float column — never reaches `Number`; [`OrderKey::of`] files it under
/// `Missing`, which is what it is: a value the style cannot rank.
pub(super) enum OrderKey<'a> {
    Missing,
    Number(f64),
    Text(&'a str),
}

impl<'a> OrderKey<'a> {
    pub(super) fn of(m: &'a Member, name: &str) -> Self {
        let Some((_, v)) = m.props.iter().find(|(k, _)| *k == name) else {
            return Self::Missing;
        };
        match v {
            PropertyRef::String(s) => Self::Text(s),
            PropertyRef::Float(f) => Self::number(f as f64),
            PropertyRef::Double(d) => Self::number(d),
            PropertyRef::Int(i) => Self::number(i as f64),
            PropertyRef::UInt(u) => Self::number(u as f64),
            PropertyRef::Bool(b) => Self::number(b as u8 as f64),
        }
    }

    /// `Number`, except that a NaN is unrankable and files under `Missing`.
    /// Keeping the invariant here means `cmp` has no incomparable case left.
    pub(super) fn number(v: f64) -> Self {
        if v.is_nan() {
            Self::Missing
        } else {
            Self::Number(v)
        }
    }

    /// A total order, so `sort_by` is well-defined: classes rank
    /// `Missing` < `Number` < `Text`, numbers by value (`total_cmp`, which is
    /// total by construction and needs no NaN case given the invariant above),
    /// text lexicographically. Equal keys are left to the caller's stable sort,
    /// which resolves them by input order.
    pub(super) fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match (self, other) {
            (Self::Missing, Self::Missing) => Ordering::Equal,
            (Self::Missing, _) => Ordering::Less,
            (_, Self::Missing) => Ordering::Greater,
            (Self::Number(a), Self::Number(b)) => a.total_cmp(b),
            (Self::Number(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Number(_)) => Ordering::Greater,
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
        }
    }
}

/// Warn when `order` names a column the layer does not publish.
///
/// Checked against the published property names rather than the raw schema, so
/// it stays right for a column whose name was restored from the rename
/// provenance (#359) and for one that is deliberately not exported.
pub(super) fn warn_if_order_column_is_absent(
    schema: &Schema,
    published: &PublishedNames,
    order: &FeatureOrder,
) {
    let FeatureOrder::Column { name, .. } = order else {
        return;
    };
    if !order_column_is_absent(schema, published, order) {
        return;
    }
    log::warn!(
        "--feature-order names {name:?}, which this layer does not publish, so \
         the order is unchanged (input row order). Available: {}",
        published_property_names(schema, published).join(", ")
    );
}

/// Whether `order` names a column the layer will not publish. `false` for
/// [`FeatureOrder::Input`], which names nothing.
pub(super) fn order_column_is_absent(
    schema: &Schema,
    published: &PublishedNames,
    order: &FeatureOrder,
) -> bool {
    let FeatureOrder::Column { name, .. } = order else {
        return false;
    };
    !published_property_names(schema, published)
        .iter()
        .any(|n| n == name)
}

/// The MVT property keys this schema will advertise.
pub(super) fn published_property_names(schema: &Schema, published: &PublishedNames) -> Vec<String> {
    let geom_idx = published
        .geometry
        .as_ref()
        .and_then(|name| schema.index_of(name).ok());
    #[cfg(test)]
    let geom_idx = geom_idx.or_else(|| geometry_index(schema));
    let Some(geom_idx) = geom_idx else {
        return Vec::new();
    };
    property_columns(schema, geom_idx, published)
        .into_iter()
        .map(|(_, n)| n)
        .collect()
}

/// Order members for encoding: tile-major always, then within each tile by
/// `order`.
///
/// The `(key, seq)` sort is load-bearing beyond aesthetics — `encode_members`
/// groups tiles by scanning runs of equal `key`, so members must stay
/// contiguous per tile. A within-tile reorder therefore runs as a second,
/// per-run pass rather than as a different global sort key.
/// Compare two members of one tile under a [`FeatureOrder::Column`] request.
///
/// The column decides, and `seq` — the source row index — breaks ties, so the
/// result is a strict order rather than merely a consistent one: the output is
/// fully determined even when the caller's sort is unstable or the input was
/// not already in `seq` order (which is the case after the oversized-tile
/// valve has ranked by vertex count). `descending` reverses the column only;
/// tied features keep input order either way.
pub(super) fn compare_by_feature_order(
    a: &Member,
    b: &Member,
    name: &str,
    descending: bool,
) -> std::cmp::Ordering {
    let ord = OrderKey::of(a, name).cmp(&OrderKey::of(b, name));
    let ord = if descending { ord.reverse() } else { ord };
    ord.then_with(|| a.seq.cmp(&b.seq))
}

pub(super) fn sort_members_for_encode(members: &mut [Member], order: &FeatureOrder) {
    members.par_sort_unstable_by_key(|m| (m.key, m.seq));
    let FeatureOrder::Column { name, descending } = order else {
        return;
    };
    // `chunk_by_mut` yields the same runs `encode_members` will group on.
    for tile in members.chunk_by_mut(|a, b| a.key == b.key) {
        tile.sort_by(|a, b| compare_by_feature_order(a, b, name, *descending));
    }
}

/// One tile's members, already in `(key, seq)` order, into feature order:
/// [`sort_members_for_encode`]'s per-tile step.
pub(super) fn sort_tile_for_encode(tile: &mut [Member], order: &FeatureOrder) {
    if let FeatureOrder::Column { name, descending } = order {
        tile.sort_by(|a, b| compare_by_feature_order(a, b, name, *descending));
    }
}

/// Record which of a tile's members reached it and which were lost, by
/// source row, when a `--feature-id` is set (#443): a source drawn in any
/// tile at any zoom is never lost.
pub(super) fn account_sources(
    members: &[Member],
    mvt: &TileMvt,
    zoom: u8,
    opts: &ExportOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<(), ExportError> {
    use crate::diagnostics::SourceLossCause;
    if opts.feature_id.is_none() {
        return Ok(());
    }
    let cause = if mvt.valve.dropped > 0 {
        SourceLossCause::ValveDropped
    } else if mvt.valve.merged > 0 {
        SourceLossCause::Merged
    } else {
        SourceLossCause::EncodeDropped
    };
    let surviving: std::collections::BTreeSet<u64> = mvt.source_ids.iter().copied().collect();
    // The merge valve owns per-source causes, including kept merges on a
    // tile that also dropped singles. Prefer those to the tile-wide default.
    let losses: std::collections::BTreeMap<u64, _> = mvt.source_losses.iter().copied().collect();
    for member in members {
        if let Some(id) = member.id {
            let cause = losses.get(&member.seq).copied().unwrap_or(cause);
            if !surviving.contains(&member.seq) {
                diagnostics.source_member_loss(member.seq, id, zoom, cause)?;
            }
        }
    }
    if mvt.features > 0 {
        for &id in &mvt.source_ids {
            diagnostics.source_emitted(id, zoom)?;
        }
    }
    Ok(())
}

/// The writer's [`EncodedTile`] for tile `x`/`y`: gzip-compressed here, in
/// the parallel encode section (#227). A tile whose every member encoded to
/// nothing (#431) is an empty carrier the writer loop skips: there is no
/// tile to write, but the drops must still reach the report.
pub(super) fn encoded_tile(
    x: u32,
    y: u32,
    mvt: TileMvt,
    fill: FillTally,
) -> Result<EncodedTile, ExportError> {
    check_mlt(&mvt)?;
    if mvt.features == 0 {
        return Ok(EncodedTile {
            x,
            y,
            data: Vec::new(),
            hash: 0,
            raw_len: 0,
            feature_count: 0,
            encode_dropped: mvt.unencodable,
            encode_quantized: mvt.quantized,
            oversized: mvt.oversized,
            valve: mvt.valve,
            // The tile is not written, so neither is its fill.
            fill: FillTally::default(),
        });
    }
    let hash = TileHasher::hash(&mvt.data);
    let raw_len = mvt.data.len();
    let data = mvt.data.into_gzip(&mvt.gzip_cache)?;
    Ok(EncodedTile {
        x,
        y,
        data,
        hash,
        raw_len,
        feature_count: mvt.features,
        encode_dropped: mvt.unencodable,
        encode_quantized: mvt.quantized,
        oversized: mvt.oversized,
        valve: mvt.valve,
        fill,
    })
}

#[cfg(test)]
pub(super) fn encode_members(
    gzip_cache: &Arc<TileGzipCache>,
    members: Vec<Member>,
    zoom: u8,
    opts: &ExportOptions,
    fills: Option<&coverage_fill::FillMap>,
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<EncodedTile>, ExportError> {
    encode_layer_members(
        gzip_cache,
        members,
        zoom,
        &TileEncoding::single(opts),
        fills,
        timers,
        diagnostics,
    )
}

pub(super) fn encode_layer_members(
    gzip_cache: &Arc<TileGzipCache>,
    mut members: Vec<Member>,
    zoom: u8,
    opts: &TileEncoding<'_>,
    fills: Option<&coverage_fill::FillMap>,
    timers: &ExportTimers,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Vec<EncodedTile>, ExportError> {
    if opts.layers.is_empty() {
        sort_members_for_encode(&mut members, &opts.feature_order);
    } else {
        // Joint reads already ordered each layer; preserve their concatenation.
        members.sort_by_key(|m| m.key);
    }

    // Runs of equal key. `members` is sorted by key above, so consecutive
    // grouping is total grouping.
    let groups: Vec<&[Member]> = members.chunk_by(|a, b| a.key == b.key).collect();

    // Encode AND gzip-compress every tile in the parallel section, so the serial
    // writer loop only dedups and appends bytes (issue #227). Compression must
    // match the writer's `tile_compression` (Gzip) so `add_tile_precompressed`
    // stays byte-identical to the old serial `add_tile_with_count` path.
    // #535: `encode` is timed per tile, inside this closure (core-seconds).
    let encode_group = |g: &[Member]| -> Option<Result<EncodedTile, ExportError>> {
        let _encode = ExportTimers::scope(&timers.encode);
        let (x, y) = key_to_xy(g[0].key, zoom);
        let tb = TileCoord::new(x, y, zoom).bounds();
        // #788: the tile's fill is owned for the encode and dropped once
        // its fill commands are built; a spilled fill's read can fail.
        let fill = fills.map(|f| {
            ExportTimers::time(&timers.fill_encode, || {
                f.get(g[0].key).map(|polys| {
                    polys.map(|polys| fill_features(&polys, g, &tb, opts, diagnostics))
                })
            })
        });
        let (fill, mut fill_tally) = match fill.transpose() {
            Ok(fill) => fill.flatten().unwrap_or_default(),
            Err(error) => return Some(Err(error)),
        };
        let mvt = encode_tile_layers(
            gzip_cache,
            g,
            &tb,
            opts,
            &fill,
            &mut fill_tally,
            diagnostics,
        );
        if let Err(error) = account_sources(g, &mvt, zoom, opts, diagnostics) {
            return Some(Err(error));
        }
        if let Some(f) = fills {
            f.note_shed(fill_tally.shed);
        }
        Some(encoded_tile(x, y, mvt, fill_tally))
    };
    // Longest tile first, one tile per task: a tile's valve passes are a
    // serial chain, so a dense tile that starts late sets the wall of its
    // level group. The results are put back in key order, so the output is
    // unchanged.
    let mut order: Vec<usize> = (0..groups.len()).collect();
    order.sort_by_key(|&i| std::cmp::Reverse(groups[i].len()));
    let mut done: Vec<(usize, Option<Result<EncodedTile, ExportError>>)> = order
        .into_par_iter()
        .with_max_len(1)
        .map(|i| (i, encode_group(groups[i])))
        .collect();
    done.sort_unstable_by_key(|(i, _)| *i);
    done.into_iter().filter_map(|(_, r)| r).collect()
}

/// `true` when `bbox` lies entirely within `tb` expanded by `buffer` on every
/// side. Clipping such a feature to the buffered tile is a geometric no-op, so
/// the clip can be skipped (and `BooleanOps` ring normalization avoided).
#[inline]
pub(super) fn bbox_within_buffered(bbox: &TileBounds, tb: &TileBounds, buffer: f64) -> bool {
    bbox.lng_min >= tb.lng_min - buffer
        && bbox.lat_min >= tb.lat_min - buffer
        && bbox.lng_max <= tb.lng_max + buffer
        && bbox.lat_max <= tb.lat_max + buffer
}

/// One tile's MVT bytes plus the encode tallies the report needs.
pub(super) struct TileMvt {
    pub(super) gzip_cache: Arc<TileGzipCache>,
    pub(super) source_ids: Vec<u64>,
    pub(super) source_losses: Vec<(u64, crate::diagnostics::SourceLossCause)>,
    /// Raw bytes and the gzip of exactly those bytes. Mutable access to the
    /// raw buffer invalidates gzip before exposing the buffer.
    pub(super) data: TileData,
    /// MVT features actually written to the layer. A `GeometryCollection`
    /// member contributes one per geometry kind it holds (#431).
    pub(super) features: usize,
    /// Members with nothing to encode — empty geometries or empty
    /// collections (#431). Content loss; gates the export warning.
    pub(super) unencodable: usize,
    /// Members with coordinates that collapsed at the tile extent — polygon
    /// rings of zero area, lines of fewer than two points (clip slivers at a
    /// buffered edge). Expected; informational only.
    pub(super) quantized: usize,
    /// Whether the oversized valve fired.
    pub(super) oversized: bool,
    /// What the size valve did to the tile (#745).
    pub(super) valve: ValveTally,
    /// Why the MVT did not convert to the export's MLT, if it did not;
    /// `data` is then empty and the export fails at this tile.
    pub(super) mlt_error: Option<String>,
}

// A read-only cached-gzip view for fill's existing cost observer. All raw
// mutation goes through TileData's invalidating mutable view.
impl std::ops::Deref for TileMvt {
    type Target = TileData;
    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

/// What the size valve did to one tile (#745), for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct ValveTally {
    /// Members merged into another member.
    pub(super) merged: usize,
    /// Members dropped: by the drop valve, or by the merge valve's fallback
    /// when merging alone could not make the tile fit.
    pub(super) dropped: usize,
    /// Whole merged features dropped, and their source-member count.
    pub(super) dropped_merged: usize,
    pub(super) dropped_merged_sources: usize,
    /// Reduced grid of the tile; per-zoom aggregate keeps the minimum.
    pub(super) reduced_extent: Option<u32>,
    pub(super) reduced_tiles: usize,
    /// Encodes the tile took, the first included: 1 for a tile that fit at
    /// once, under either valve.
    pub(super) passes: u32,
    /// The merge threshold the tile ended on.
    pub(super) mingap: u64,
    /// Tiles the valve left over the limit: 0 or 1 per tile, summed (#814).
    pub(super) over_limit: usize,
    /// The over-cap tile's lone survivor absorbed another source feature.
    pub(super) over_limit_merged: bool,
}

/// One #750 coverage-fill feature of a tile: the fill polygons that joined
/// one of the tile's features, drawn with that feature's id and tags. Built
/// once per tile; the fill then takes only the room the size valve leaves
/// ([`with_fill`]).
pub(super) struct FillFeature {
    pub(super) id: Option<u64>,
    pub(super) source_row: u64,
    pub(super) props: SharedProps,
    pub(super) cmds: Vec<u32>,
    /// Its polygons and their vertices, for the report.
    pub(super) polygons: usize,
    pub(super) vertices: usize,
    /// Its area in tile units², which decides what is shed first.
    pub(super) area: f64,
}

impl FillFeature {
    pub(super) fn encode_fill(
        id: Option<u64>,
        source_row: u64,
        props: SharedProps,
        polys: &[&coverage_fill::FillPolygon],
        diagnostics: &crate::diagnostics::Diagnostics,
    ) -> Self {
        #[cfg(test)]
        coverage_fill::cost_probe::update(|c| c.fill_polygons_encoded += polys.len());
        Self {
            id,
            source_row,
            props,
            cmds: crate::mvt::encode_tile_unit_polygons(polys.iter().copied(), diagnostics),
            polygons: polys.len(),
            vertices: polys.iter().flat_map(|p| p.iter()).map(Vec::len).sum(),
            area: polys.iter().map(|p| coverage_fill::poly_area(p)).sum(),
        }
    }
}

/// What a tile's coverage fill drew (#750), for the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct FillTally {
    /// Fill polygons drawn.
    pub(super) polygons: usize,
    /// Their vertices, holes included.
    pub(super) vertices: usize,
    /// Fill polygons left out to stay within [`fill_tile_bytes`], or to leave
    /// the tile's own features their room ([`with_fill`]).
    pub(super) shed: usize,
}

/// A tile's fill features: every fill polygon joins the member whose tile-unit
/// bbox holds it ([`coverage_fill::assign_hosts`]), and each host's polygons
/// are encoded once as one MultiPolygon.
pub(super) fn fill_features(
    polys: &[coverage_fill::FillPolygon],
    members: &[Member],
    tb: &TileBounds,
    opts: &ExportOptions,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> (Vec<FillFeature>, FillTally) {
    if members.is_empty() || polys.is_empty() {
        return (Vec::new(), FillTally::default());
    }
    let (kept, shed) = coverage_fill::within_budget(polys, fill_tile_bytes(opts));
    let polys: Vec<&coverage_fill::FillPolygon> = kept.into_iter().map(|i| &polys[i]).collect();
    let polys = polys.as_slice();
    let tally = FillTally {
        polygons: polys.len(),
        vertices: polys.iter().flat_map(|p| p.iter()).map(Vec::len).sum(),
        shed,
    };
    let proj = crate::mvt::TileProjector::new(tb, opts.extent);
    // A polygon fill takes a polygon's tags when the tile has polygons: a
    // road's or a point's would colour it by the wrong attributes.
    let polygonal =
        |m: &Member| matches!(*m.geom, Geometry::Polygon(_) | Geometry::MultiPolygon(_));
    let any_polygon = members.iter().any(polygonal);
    let hosts: Vec<[f64; 4]> = members
        .iter()
        .map(|m| match m.geom.bounding_rect() {
            Some(_) if any_polygon && !polygonal(m) => [f64::INFINITY; 4],
            Some(r) => {
                let (x0, y0) = proj.project(r.min().x, r.max().y);
                let (x1, y1) = proj.project(r.max().x, r.min().y);
                [x0, y0, x1, y1]
            }
            None => [f64::INFINITY; 4],
        })
        .collect();
    let mut groups: std::collections::BTreeMap<usize, Vec<&coverage_fill::FillPolygon>> =
        std::collections::BTreeMap::new();
    for (poly, host) in polys
        .iter()
        .copied()
        .zip(coverage_fill::assign_hosts(polys.iter().copied(), &hosts))
    {
        groups.entry(host).or_default().push(poly);
    }
    let feats = groups
        .into_iter()
        .map(|(host, group)| {
            FillFeature::encode_fill(
                members[host].id,
                members[host].seq,
                members[host].props.clone(),
                &group,
                diagnostics,
            )
        })
        .filter(|f| !f.cmds.is_empty())
        .collect();

    (feats, tally)
}

/// Tile-byte allowance for a tile's #750 fill: a quarter of its size limit,
/// with a compressed limit read as about twice that many raw bytes. This is
/// separate from the memory workspace granted under `MemoryPhase::Fill`.
/// Unlimited without a limit. At half the limit the size valve ran up to
/// ten passes per dense tile squeezing the tile's own features into the
/// rest, each pass re-encoding the whole tile; at a quarter, coverage on FTW
/// Moldova was the same and the export 27% faster.
pub(super) fn fill_tile_bytes(opts: &ExportOptions) -> usize {
    let raw = opts.tile_size_limit.filter(|&l| l > 0);
    let gz = opts
        .compressed_tile_size_limit
        .filter(|&l| l > 0)
        .map(|l| l.saturating_mul(2));
    match (raw, gz) {
        (Some(a), Some(b)) => a.min(b) / 4,
        (Some(a), None) | (None, Some(a)) => a / 4,
        (None, None) => usize::MAX,
    }
}

/// What [`build_mvt`] needs from a tile feature: a plain member, or a member
/// with others merged into it (#745).
pub(super) trait TileFeature: Sync {
    fn geometry(&self) -> &Geometry<f64>;
    fn coords_count(&self) -> usize {
        self.geometry().coords_count()
    }
    fn properties(&self) -> &SharedProps;
    fn feature_id(&self) -> Option<u64>;
    fn source_row(&self) -> u64;
    fn band(&self) -> u16;
}

impl TileFeature for Member {
    fn band(&self) -> u16 {
        #[cfg(test)]
        JOINT_BAND_READS.with(|n| n.set(n.get() + 1));
        self.band
    }
    fn geometry(&self) -> &Geometry<f64> {
        &self.geom
    }
    fn properties(&self) -> &SharedProps {
        &self.props
    }
    fn feature_id(&self) -> Option<u64> {
        self.id
    }
    fn source_row(&self) -> u64 {
        self.seq
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static JOINT_BAND_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl TileFeature for coalesce_valve::Merged<'_> {
    fn band(&self) -> u16 {
        self.target.band
    }
    fn geometry(&self) -> &Geometry<f64> {
        self.geometry()
    }
    fn coords_count(&self) -> usize {
        self.coords_count()
    }
    fn properties(&self) -> &SharedProps {
        self.props()
    }
    fn feature_id(&self) -> Option<u64> {
        self.target.id
    }
    fn source_row(&self) -> u64 {
        self.target.seq
    }
}

impl<T: TileFeature + ?Sized> TileFeature for &T {
    fn band(&self) -> u16 {
        (**self).band()
    }
    fn geometry(&self) -> &Geometry<f64> {
        (**self).geometry()
    }
    fn coords_count(&self) -> usize {
        (**self).coords_count()
    }
    fn properties(&self) -> &SharedProps {
        (**self).properties()
    }
    fn feature_id(&self) -> Option<u64> {
        (**self).feature_id()
    }
    fn source_row(&self) -> u64 {
        (**self).source_row()
    }
}

/// The valve's export options and optional joint layer mapping. A normal
/// export uses an empty mapping and the original single-layer build path.
pub(super) struct TileEncoding<'a> {
    pub(super) options: &'a ExportOptions,
    pub(super) layers: &'a [(u16, String)],
}

impl<'a> TileEncoding<'a> {
    /// One layer named by the options, the ordinary export.
    pub(super) fn single(options: &'a ExportOptions) -> Self {
        Self {
            options,
            layers: &[],
        }
    }
}

impl std::ops::Deref for TileEncoding<'_> {
    type Target = ExportOptions;
    fn deref(&self) -> &Self::Target {
        self.options
    }
}

/// Encode a single tile's members to MVT bytes, applying the size valve.
///
/// The valve fits the tile's own features alone, exactly as without a #750
/// fill; the fill then takes only the room they leave ([`with_fill`]), so
/// it never makes the valve merge or drop a real feature. `fill_tally`
/// counts what of the fill is drawn and shed.
#[cfg(test)]
pub(super) fn encode_tile(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &ExportOptions,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    encode_tile_layers(
        gzip_cache,
        members,
        tb,
        &TileEncoding::single(opts),
        fill,
        fill_tally,
        diagnostics,
    )
}

pub(super) fn encode_tile_layers(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[Member],
    tb: &TileBounds,
    opts: &TileEncoding<'_>,
    fill: &[FillFeature],
    fill_tally: &mut FillTally,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    if opts.size_valve == SizeValve::Merge && !is_point_like(members) {
        return merge_to_fit(gzip_cache, members, tb, opts, fill, fill_tally, diagnostics);
    }
    drop_to_fit(gzip_cache, members, tb, opts, fill, fill_tally, diagnostics)
}

/// Retained input for the merge's joint valve. No overview is opened until a
/// joined tile exceeds a cap. Cell bands can retain one source per zoom.
#[derive(Clone)]
pub(crate) struct JointSource {
    pub path: PathBuf,
    pub options: ExportOptions,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub members: JointMembers,
}

/// The source export's scan counts and resident member price, before valves
/// shed or merge members. Tile ids already include the zoom.
#[derive(Clone, Default)]
pub(crate) struct JointMembers {
    pub(super) tile_counts: BTreeMap<u64, usize>,
    pub(super) member_bytes: Option<u64>,
}

/// A merge's retained overview plans. Footers, columns, CRS and zoom ladders
/// are resolved once, then reused by every zoom's restricted wave reads.
pub(crate) struct JoinedTileFitter<'a> {
    pub(super) sources: Vec<OpenedJointSource<'a>>,
}

pub(super) struct OpenedJointSource<'a> {
    pub(super) source: &'a JointSource,
    pub(super) reader: OverviewReader,
    pub(super) columns: ResolvedColumns,
    pub(super) crs: Crs,
    pub(super) zooms: Vec<u8>,
    /// #802: the finest level's pins, as the source export built them, so a
    /// refitted max-zoom tile keeps the borders its source export kept.
    pub(super) pins: Option<JunctionNodes>,
}

impl<'a> JoinedTileFitter<'a> {
    /// Resolve once per pending zoom, before the merge sizes its chunks.
    pub(crate) fn tile_wave(&self, tiles: &BTreeSet<u64>, opts: &ExportOptions) -> usize {
        #[cfg(test)]
        joined_tile_probe::PROFILE.with(|profile| profile.set(Some(opts.profile)));
        self.tile_wave_under(tiles, opts, &export_budget())
    }

    pub(super) fn tile_wave_under(
        &self,
        tiles: &BTreeSet<u64>,
        opts: &ExportOptions,
        budget: &MemoryBudget,
    ) -> usize {
        joined_tile_wave_under(
            tiles,
            self.sources.iter().map(|s| &s.source.members),
            opts,
            budget,
        )
    }

    /// The joins this fitter may refit; the rest are reported and left to
    /// the source valves (see [`joined_tiles_within`]).
    pub(crate) fn within_memory(
        &self,
        tiles: BTreeSet<u64>,
        diagnostics: &crate::diagnostics::Diagnostics,
    ) -> BTreeSet<u64> {
        let limit = probe_oom_memory_limit().filter(|limit| {
            limit.source != MemoryLimitSource::Override && !skip_memory_preflight_from_env()
        });
        let Some(limit) = limit else {
            return tiles;
        };
        let fit = joined_tiles_within(
            &tiles,
            self.sources.iter().map(|s| &s.source.members),
            limit,
        );
        if let Some(first) = tiles.difference(&fit).next() {
            diagnostics.warn(format!(
                "{} joined tile(s) over a size cap (first id {first}) were not refitted: \
                 their members across layers exceed the {} of {} bytes",
                tiles.len() - fit.len(),
                limit.source.describe(),
                limit.bytes
            ));
        }
        fit
    }

    pub(crate) fn open(
        sources: impl IntoIterator<Item = &'a JointSource>,
    ) -> Result<Self, ExportError> {
        let started = Instant::now();
        let replay = &crate::diagnostics::Diagnostics::silent();
        let sources = sources
            .into_iter()
            .map(|source| {
                let reader = OverviewReader::open(&source.path)?;
                let zooms = resolve_level_zooms(reader.meta(), reader.num_levels())?;
                let band = BandRead {
                    source: reader.source(),
                    budget: export_budget(),
                    profile: source.options.profile,
                };
                let columns = resolve_export_columns((&reader, band), &source.options, &zooms)?;
                let crs = crs_from_descriptor(reader.resolved_schema(), &source.path, replay)?;
                let pins = finest_level_pins(
                    (&reader, band),
                    crs,
                    &source.options,
                    &mut export_budget(),
                    replay,
                )?;
                Ok(OpenedJointSource {
                    source,
                    reader,
                    columns,
                    crs,
                    zooms,
                    pins,
                })
            })
            .collect::<Result<Vec<_>, ExportError>>()?;
        log::debug!(
            "[profile] joined plans: {} overview(s), {:.3}s",
            sources.len(),
            started.elapsed().as_secs_f64()
        );
        Ok(Self { sources })
    }

    /// Fit one wave of joins. The merge releases its replacements before
    /// requesting another wave. Only valve counts from replay are published.
    pub(crate) fn fit_joined_tiles(
        &self,
        zoom: u8,
        tiles: &BTreeSet<u64>,
        opts: &ExportOptions,
        diagnostics: &crate::diagnostics::Diagnostics,
    ) -> Result<BTreeMap<u64, JoinedTile>, ExportError> {
        #[cfg(test)]
        joined_tile_probe::ZOOMS.with(|zooms| zooms.set(zooms.get() | (1 << zoom)));
        let started = Instant::now();
        let replay = &crate::diagnostics::Diagnostics::silent();
        // The replay's discarded loss ledger still spills; keep it where the
        // source exports spilled, never the system temp directory.
        replay.set_accounting_spill_dir(
            self.sources
                .iter()
                .find_map(|s| s.source.options.spill_dir.as_deref()),
        );
        let cache = Arc::new(TileGzipCache::new(8 * 1024 * 1024));
        let timers = ExportTimers::default();
        // A partition per requested tile gives the existing encoder parallel
        // valve chains, while a wave reads each layer only once. The tile-set
        // restriction also prunes disjoint bboxes and prevents clipping holes
        // between requested ids, as in variable-depth and shard exports.
        let tile_counts = tiles.iter().map(|&id| (id, 1)).collect();
        let plan = LevelPlan::cut(
            zoom,
            plan_partitions(&tile_counts, zoom, 1),
            self.tile_wave(tiles, opts),
            usize::MAX,
        );
        let mut replacements = BTreeMap::new();
        for wave in plan.waves() {
            let mut buckets: Vec<Vec<Member>> = (0..wave.len()).map(|_| Vec::new()).collect();
            let mut layers = Vec::new();
            let mut seq_base = 0;
            for OpenedJointSource {
                source,
                reader,
                columns,
                crs,
                zooms,
                pins,
            } in &self.sources
            {
                if zoom < source.min_zoom || zoom > source.max_zoom {
                    continue;
                }
                let Some(level_idx) = zooms.iter().position(|&z| z == zoom) else {
                    continue;
                };
                let ctx = LevelCtx {
                    reader,
                    band: BandRead {
                        source: reader.source(),
                        budget: export_budget(),
                        profile: source.options.profile,
                    },
                    level_idx,
                    crs: *crs,
                    zoom,
                    opts: &source.options,
                    gzip_cache: &cache,
                    published: &columns.published,
                    feature_id: columns.feature_id.as_ref(),
                    timers: &timers,
                    fills: None,
                    tiles: Some(&tile_counts),
                    pins: pins.as_ref(),
                    #[cfg(test)]
                    work: &Mutex::new(ExportWorkTally::default()),
                };
                let collected = collect_wave(&ctx, wave, replay)?.buckets;
                let mut bands = BTreeMap::new();
                let mut next_seq = seq_base;
                for (bucket, mut members) in buckets.iter_mut().zip(collected) {
                    sort_members_for_encode(&mut members, &source.options.feature_order);
                    for m in &mut members {
                        let next_band = u16::try_from(layers.len()).map_err(|_| {
                            ExportError::Io(std::io::Error::other("too many joint valve bands"))
                        })?;
                        m.band = *bands.entry(m.band).or_insert_with(|| {
                            layers.push((next_band, source.options.layer_name.clone()));
                            next_band
                        });
                        m.seq += seq_base;
                        next_seq = next_seq.max(m.seq + 1);
                    }
                    bucket.extend(members);
                }
                seq_base = next_seq;
            }
            // DIVERGENCE FROM TIPPECANOE: gaps and predecessors remain local to
            // each overview row group, with concatenated layer sequences rather
            // than one global spatial stream (tile.cpp:1176-1195, 2016-2050).
            let encoded = encode_buckets(
                &cache,
                (buckets, None),
                zoom,
                &TileEncoding {
                    options: opts,
                    layers: &layers,
                },
                None,
                &timers,
                replay,
            )?;
            for tile in encoded.into_iter().flatten() {
                diagnostics.record(crate::diagnostics::Diagnostic::Merged {
                    count: tile.valve.merged,
                });
                diagnostics.record(crate::diagnostics::Diagnostic::ValveDropped {
                    count: tile.valve.dropped,
                });
                replacements.insert(
                    tile_key(tile.x, tile.y, zoom),
                    JoinedTile {
                        data: tile.data,
                        raw_len: tile.raw_len,
                        merged: tile.valve.over_limit_merged,
                        #[cfg(test)]
                        _live: joined_tile_probe::LiveTile::new(),
                    },
                );
            }
        }
        let stages = timers.stage_secs();
        log::debug!(
            "[profile] joined z{zoom}: {} tile(s), wall={:.3}s; \
             core-seconds read={:.3} decode={:.3} clip={:.3} encode={:.3}",
            tiles.len(),
            started.elapsed().as_secs_f64(),
            stages.band_read,
            stages.decode,
            stages.clip,
            stages.encode,
        );
        if opts.feature_id.is_some() && !replacements.is_empty() {
            // shortcut: the per-layer loss ledgers are sealed before the merge,
            // so a joint refit's drops are not in them; feed them in if needed.
            diagnostics.warn(
                "--feature-id: joined tiles over a size cap were refitted across layers; \
                 the source losses list each layer's own export and miss the joint \
                 valve's merges and drops"
                    .to_string(),
            );
        }
        Ok(replacements)
    }
}

/// Price the densest pending tile from original overview members, summed
/// across sources, using the same resident model and budget as level waves.
pub(super) fn joined_tile_wave_under<'a>(
    tiles: &BTreeSet<u64>,
    sources: impl Iterator<Item = &'a JointMembers> + Clone,
    opts: &ExportOptions,
    budget: &MemoryBudget,
) -> usize {
    let resident = tiles
        .iter()
        .map(|&id| joined_tile_resident(id, sources.clone(), opts))
        .max()
        .unwrap_or(0);
    resolve_partition_wave_under(opts.partition_wave, opts.profile, budget).min(budget.affordable(
        opts.profile,
        MemoryPhase::ExportCollect,
        Price::new(resident, PriceBasis::Footer),
    ))
}

/// A joined tile's members across its sources, priced as an export prices one.
pub(super) fn joined_tile_resident<'a>(
    id: u64,
    sources: impl Iterator<Item = &'a JointMembers>,
    opts: &ExportOptions,
) -> u64 {
    sources.fold(0u64, |total, source| {
        let per_member = source
            .member_bytes
            .map_or(0, |bytes| export_member_resident_bytes(bytes, opts));
        let count = source.tile_counts.get(&id).copied().unwrap_or(0) as u64;
        total.saturating_add(count.saturating_mul(per_member))
    })
}

/// The joins whose members fit `limit` together, priced as [`check_levels_fit`]
/// prices a tile. An export refuses a tile that cannot (#665), but each source
/// export already wrote its part, so a join that cannot is not refitted: it
/// keeps the source valves' result and the over-cap warning instead of
/// risking an OOM kill.
pub(super) fn joined_tiles_within<'a>(
    tiles: &BTreeSet<u64>,
    sources: impl Iterator<Item = &'a JointMembers> + Clone,
    limit: MemoryLimit,
) -> BTreeSet<u64> {
    tiles
        .iter()
        .copied()
        .filter(|id| {
            let resident = sources.clone().fold(0u64, |total, source| {
                let count = source.tile_counts.get(id).copied().unwrap_or(0) as u64;
                total.saturating_add(count.saturating_mul(source.member_bytes.unwrap_or(0)))
            });
            resident <= limit.bytes
        })
        .collect()
}

/// A fitted join, already gzipped by the parallel export encoder.
pub(crate) struct JoinedTile {
    pub data: Vec<u8>,
    pub raw_len: usize,
    pub merged: bool,
    #[cfg(test)]
    _live: joined_tile_probe::LiveTile,
}

/// Vertices at which [`build_mvt`] encodes a tile's geometries in parallel
/// (#647). Coarse zooms hold a few tiles with thousands of members each (a
/// single z3 tile of the Madagascar fixture is 489 KB), and the per-tile
/// parallel section in [`encode_members`] runs each of those on one thread.
///
/// The encode cost follows the vertex count, not the member count, so the
/// threshold counts vertices. On an idle 16-core pool the parallel encode
/// broke even at about 500–1,000 vertices: 528 vertices took 26 µs serial
/// and 24 µs parallel, and 1,028 took 38 µs and 27 µs. At 4,096 vertices it
/// was 1.4–3.5× faster. The threshold sits well above break-even because
/// in production the other cores are often busy with other tiles.
pub(super) const PARALLEL_ENCODE_MIN_COORDS: usize = 4096;

/// Whether [`build_mvt`] encodes these members' geometries in parallel: two
/// or more members holding at least [`PARALLEL_ENCODE_MIN_COORDS`] vertices.
/// The count stops at the threshold, so a small tile costs one short scan.
pub(super) fn encode_in_parallel<M: TileFeature>(members: &[M]) -> bool {
    if members.len() < 2 {
        return false;
    }
    let mut coords = 0usize;
    for m in members {
        coords += m.coords_count();
        if coords >= PARALLEL_ENCODE_MIN_COORDS {
            return true;
        }
    }
    false
}

/// Build the MVT bytes for a sequence of tile members.
///
/// The geometry encode (projection, quantization, ring cleaning) is pure per
/// member, so on a tile that [`encode_in_parallel`] selects it runs in
/// parallel. Tag interning stays serial, in member order, which keeps the
/// key/value tables and the bytes identical to the serial loop. A smaller
/// tile encodes and interns each member in turn, with no buffer.
pub(super) fn build_mvt<M: TileFeature>(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[M],
    tb: &TileBounds,
    opts: &ExportOptions,
    fill: &[&FillFeature],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    build_mvt_with(gzip_cache, members, opts, fill, |m| {
        EncodedFeatureGeometry::encode(m.geometry(), tb, opts.extent, diagnostics)
    })
}

// One encoder for both valves. The ordinary export takes the original build
// path; only a joint retry supplies a band-to-layer mapping.
pub(super) fn build_tile_layers<M: TileFeature>(
    cache: &Arc<TileGzipCache>,
    layers: &[(u16, String)],
    members: &[M],
    tb: &TileBounds,
    opts: &ExportOptions,
    fill: &[&FillFeature],
    diagnostics: &crate::diagnostics::Diagnostics,
) -> TileMvt {
    if layers.is_empty() {
        return build_mvt(cache, members, tb, opts, fill, diagnostics);
    }
    build_tile_layers_with(cache, layers, members, opts, fill, |m| {
        EncodedFeatureGeometry::encode(m.geometry(), tb, opts.extent, diagnostics)
    })
}

pub(super) fn build_tile_layers_with<M: TileFeature>(
    cache: &Arc<TileGzipCache>,
    layers: &[(u16, String)],
    members: &[M],
    opts: &ExportOptions,
    fill: &[&FillFeature],
    encode: impl Fn(&M) -> EncodedFeatureGeometry + Sync,
) -> TileMvt {
    if layers.is_empty() {
        return build_mvt_with(cache, members, opts, fill, encode);
    }
    debug_assert!(fill.is_empty());
    let mut out = build_mvt_with(cache, &[] as &[M], opts, &[], &encode);
    let mut bytes = Vec::new();
    // Group in one member walk, preserving both the first occurrence of
    // each layer and the draw order within it. Rescanning all members for
    // every layer, with a band-list search per member, was quadratic in
    // the number of layers on every valve pass.
    let mut grouped: Vec<(&String, Vec<&M>)> = Vec::new();
    let mut bands = BTreeMap::new();
    for (band, name) in layers {
        let index = grouped
            .iter()
            .position(|(n, _)| *n == name)
            .unwrap_or_else(|| {
                grouped.push((name, Vec::new()));
                grouped.len() - 1
            });
        bands.insert(*band, index);
    }
    for member in members {
        if let Some(&index) = bands.get(&member.band()) {
            grouped[index].1.push(member);
        }
    }
    grouped.retain(|(_, members)| !members.is_empty());
    // Layers encode independently; the joint valve re-encodes all of them on
    // every pass, so build them in parallel and join in layer order.
    let built: Vec<TileMvt> = grouped
        .into_par_iter()
        .map(|(name, selected)| {
            let layer_opts = ExportOptions {
                layer_name: name.clone(),
                tile_format: TileFormat::Mvt,
                ..opts.clone()
            };
            build_mvt_with(cache, &selected, &layer_opts, &[], |m| encode(*m))
        })
        .collect();
    for layer in built {
        bytes.extend_from_slice(&layer.data);
        out.features += layer.features;
        out.unencodable += layer.unencodable;
        out.quantized += layer.quantized;
        out.source_ids.extend(layer.source_ids);
        out.source_losses.extend(layer.source_losses);
    }
    let (data, error) =
        match crate::tile_format::finish_tile(bytes, opts.tile_format, opts.mlt_profile) {
            Ok(data) => (data, None),
            Err(e) => (Vec::new(), Some(e)),
        };
    out.data = data.into();
    out.mlt_error = error;
    out
}

/// [`build_mvt`] with each feature's geometry encoded by `encode`, which must
/// return what [`EncodedFeatureGeometry::encode`] would (#758: the merge
/// valve's cached encode).
pub(super) fn build_mvt_with<M: TileFeature>(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[M],
    opts: &ExportOptions,
    fill: &[&FillFeature],
    encode: impl Fn(&M) -> EncodedFeatureGeometry + Sync,
) -> TileMvt {
    build_mvt_within(gzip_cache, members, opts, fill, encode, usize::MAX)
        .expect("no limit to exceed")
}

/// [`build_mvt_with`], given up (`None`) as soon as the layer's encoded
/// features and tables pass `raw_limit` bytes: the tile can only grow, so
/// it is already over a raw cap of that size (#803's probe).
pub(super) fn build_mvt_within<M: TileFeature>(
    gzip_cache: &Arc<TileGzipCache>,
    members: &[M],
    opts: &ExportOptions,
    fill: &[&FillFeature],
    encode: impl Fn(&M) -> EncodedFeatureGeometry + Sync,
    raw_limit: usize,
) -> Option<TileMvt> {
    // The MVT's size says nothing of the MLT's, so only an MVT build can
    // give up early; an MLT tile is measured after conversion.
    let raw_limit = match opts.tile_format {
        TileFormat::Mvt => raw_limit,
        _ => usize::MAX,
    };
    let mut source_ids = Vec::new();
    let mut source_losses = Vec::new();
    let mut track = |id: Option<u64>, geom: &EncodedFeatureGeometry| {
        use crate::diagnostics::SourceLossCause;
        if let Some(id) = id {
            match geom {
                EncodedFeatureGeometry::Parts(_) => source_ids.push(id),
                EncodedFeatureGeometry::Unencodable => {
                    source_losses.push((id, SourceLossCause::EncodeDropped))
                }
                EncodedFeatureGeometry::Quantized => {
                    source_losses.push((id, SourceLossCause::Quantized))
                }
            }
        }
    };
    let mut layer = crate::mvt::StreamingLayerBuilder::new(&opts.layer_name, opts.extent);
    // #750: the coverage fill comes first, so a renderer draws it under the
    // tile's own features. Each fill feature carries its host's id and tags.
    for f in fill {
        let geom = EncodedFeatureGeometry::Parts(vec![(f.cmds.clone(), GeomType::Polygon)]);
        track(f.id.map(|_| f.source_row), &geom);
        layer.add_borrowed_feature(f.id, geom, f.props.iter());
    }
    if encode_in_parallel(members) {
        // Bound the geometry staging buffer as well as the serialized layer.
        // Intern and emit each chunk in original member order; only geometry
        // projection runs in parallel, exactly as before.
        const GEOMETRY_CHUNK_MEMBERS: usize = 4096;
        for chunk in members.chunks(GEOMETRY_CHUNK_MEMBERS) {
            let encoded: Vec<EncodedFeatureGeometry> = chunk.par_iter().map(&encode).collect();
            for (m, geom) in chunk.iter().zip(encoded) {
                track(m.feature_id().map(|_| m.source_row()), &geom);
                layer.add_borrowed_feature(m.feature_id(), geom, m.properties().iter());
            }
            if layer.encoded_len() > raw_limit {
                return None;
            }
        }
    } else {
        for m in members {
            let geom = encode(m);
            track(m.feature_id().map(|_| m.source_row()), &geom);
            layer.add_borrowed_feature(m.feature_id(), geom, m.properties().iter());
            if layer.encoded_len() > raw_limit {
                return None;
            }
        }
    }
    // The fill is drawn but is not the data's own features: the report,
    // the tilestats and the empty-tile check count members only (#750).
    let features = layer.feature_count() - fill.len();
    let unencodable = layer.dropped_features();
    let quantized = layer.quantized_features();
    let (data, mlt_error) =
        match crate::tile_format::finish_tile(layer.build(), opts.tile_format, opts.mlt_profile) {
            Ok(data) => (data, None),
            Err(e) => (Vec::new(), Some(e)),
        };
    Some(TileMvt {
        gzip_cache: Arc::clone(gzip_cache),
        source_ids,
        source_losses,
        data: data.into(),
        features,
        unencodable,
        quantized,
        oversized: false,
        valve: ValveTally::default(),
        mlt_error,
    })
}

/// Observe actual replacement lifetimes on the merge's thread, including
/// unwritten replacements retained across calls to the fitter.
#[cfg(test)]
pub(crate) mod joined_tile_probe {
    use std::cell::Cell;

    thread_local! {
        static COUNTS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
        pub(super) static PROFILE: Cell<Option<super::MemoryProfile>> = const { Cell::new(None) };
        pub(super) static ZOOMS: Cell<u32> = const { Cell::new(0) };
    }

    pub(crate) fn reset() {
        COUNTS.with(|counts| counts.set((0, 0)));
        PROFILE.with(|profile| profile.set(None));
        ZOOMS.with(|zooms| zooms.set(0));
    }

    pub(crate) fn counts() -> (usize, usize) {
        COUNTS.with(Cell::get)
    }

    pub(crate) fn profile() -> Option<super::MemoryProfile> {
        PROFILE.with(Cell::get)
    }

    pub(crate) fn zooms() -> u32 {
        ZOOMS.with(Cell::get)
    }

    pub(super) struct LiveTile;

    impl LiveTile {
        pub(super) fn new() -> Self {
            COUNTS.with(|counts| {
                let (live, peak) = counts.get();
                counts.set((live + 1, peak.max(live + 1)));
            });
            Self
        }
    }

    impl Drop for LiveTile {
        fn drop(&mut self) {
            COUNTS.with(|counts| {
                let (live, peak) = counts.get();
                counts.set((live - 1, peak));
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use geo::{LineString, Point};

    use super::super::tests::*;
    use super::*;

    // ========================================================================
    // Within-tile feature order (#361)
    // ========================================================================

    fn ordered_member(seq: u64, level: f64) -> Member {
        Member {
            key: 0,
            seq,
            geom: Geometry::Point(Point::new(seq as f64 * 0.001, 0.0)).into(),
            props: SharedProps::new(vec![("level".into(), PropertyValue::Double(level))]),
            id: None,
            gap: GAP_NONE,
            band: 0,
        }
    }

    /// Draw order within a tile, as the renderer sees it: `build_mvt` emits
    /// features in iteration order, so this is the paint sequence.
    fn paint_order(members: Vec<Member>, order: &FeatureOrder) -> Vec<u64> {
        let mut m = members;
        sort_members_for_encode(&mut m, order);
        m.iter().map(|x| x.seq).collect()
    }

    /// The default is input order, which is what tylertoo has always done and
    /// what the file's row order already encodes. Pinning it in a test makes
    /// the guarantee something callers can rely on rather than an accident.
    #[test]
    fn default_feature_order_is_input_row_order() {
        let members = vec![
            ordered_member(7, 0.5),
            ordered_member(2, 0.2),
            ordered_member(5, 0.35),
        ];
        assert_eq!(paint_order(members, &FeatureOrder::Input), vec![2, 5, 7]);
    }

    /// #361: styling a nested stack needs the small hot cores painted last.
    /// Ascending on the ramp column does that without every downstream style
    /// having to carry a `fill-sort-key` workaround.
    #[test]
    fn column_feature_order_sorts_ascending_within_the_tile() {
        let members = vec![
            ordered_member(0, 0.5),
            ordered_member(1, 0.2),
            ordered_member(2, 0.425),
            ordered_member(3, 0.275),
        ];
        let order = FeatureOrder::Column {
            name: "level".to_string(),
            descending: false,
        };
        // 0.2, 0.275, 0.425, 0.5 ascending → the highest value ends up on top.
        assert_eq!(paint_order(members, &order), vec![1, 3, 2, 0]);
    }

    #[test]
    fn column_feature_order_sorts_descending_when_asked() {
        let members = vec![
            ordered_member(0, 0.5),
            ordered_member(1, 0.2),
            ordered_member(2, 0.425),
        ];
        let order = FeatureOrder::Column {
            name: "level".to_string(),
            descending: true,
        };
        assert_eq!(paint_order(members, &order), vec![0, 2, 1]);
    }

    /// Ties keep input order, so the result is fully determined — two runs of
    /// the same build produce byte-identical tiles.
    #[test]
    fn column_feature_order_breaks_ties_by_input_order() {
        let members = vec![
            ordered_member(9, 0.2),
            ordered_member(4, 0.2),
            ordered_member(6, 0.2),
        ];
        let order = FeatureOrder::Column {
            name: "level".to_string(),
            descending: false,
        };
        assert_eq!(paint_order(members, &order), vec![4, 6, 9]);
    }

    /// A feature missing the column sorts before every value that has one
    /// (painted underneath), rather than being dropped or aborting the export.
    #[test]
    fn features_missing_the_order_column_sort_first() {
        let mut bare = ordered_member(0, 0.0);
        bare.props = SharedProps::new(Vec::new());
        let members = vec![ordered_member(1, 0.5), bare, ordered_member(2, 0.2)];
        let order = FeatureOrder::Column {
            name: "level".to_string(),
            descending: false,
        };
        assert_eq!(paint_order(members, &order), vec![0, 2, 1]);
    }

    /// A NaN in the ordering column must not abort the export.
    ///
    /// Treating an incomparable pair as *equal* does not give a total order:
    /// with `1.0 == NaN` and `NaN == 2.0` but `1.0 < 2.0`, equality is not
    /// transitive, and `slice::sort_by` is documented to panic when it detects
    /// that ("user-provided comparison function does not correctly implement a
    /// total order"). A NaN is ordinary in a float column read from parquet —
    /// it is how plenty of sources spell nodata — so `--feature-order` must
    /// give it a defined place instead.
    #[test]
    fn nan_in_the_order_column_does_not_abort_the_sort() {
        // Enough members, with NaN interleaved, to reach the merge path where
        // the standard library checks the comparator's consistency.
        let members: Vec<Member> = (0..64)
            .map(|i| {
                ordered_member(
                    i,
                    if i.is_multiple_of(3) {
                        f64::NAN
                    } else {
                        (64 - i) as f64
                    },
                )
            })
            .collect();
        let order = FeatureOrder::Column {
            name: "level".to_string(),
            descending: false,
        };
        let painted = paint_order(members, &order);
        assert_eq!(painted.len(), 64, "no feature may be lost");

        // The ranked features must still come out ranked. NaN joins the
        // unrankable features underneath, so every NaN precedes every number.
        let level_of = |seq: u64| {
            if seq.is_multiple_of(3) {
                f64::NAN
            } else {
                (64 - seq) as f64
            }
        };
        let ranked: Vec<f64> = painted
            .iter()
            .map(|s| level_of(*s))
            .filter(|v| !v.is_nan())
            .collect();
        assert!(
            ranked.windows(2).all(|w| w[0] <= w[1]),
            "the comparable values must be ascending, got {ranked:?}"
        );
        let first_number = painted.iter().position(|s| !level_of(*s).is_nan()).unwrap();
        assert!(
            painted[..first_number]
                .iter()
                .all(|s| level_of(*s).is_nan()),
            "NaN sorts with the unrankable features, underneath"
        );
    }

    /// Ordering is per tile: a sort must never move a member across tile keys,
    /// or `encode_members`' run-grouping would silently split tiles.
    #[test]
    fn column_feature_order_never_reorders_across_tiles() {
        let mk = |key: u64, seq: u64, level: f64| Member {
            key,
            seq,
            geom: Geometry::Point(Point::new(0.0, 0.0)).into(),
            props: SharedProps::new(vec![("level".into(), PropertyValue::Double(level))]),
            id: None,
            gap: GAP_NONE,
            band: 0,
        };
        // Tile 1's low value must not migrate ahead of tile 0's high value.
        let mut members = vec![mk(1, 0, 0.1), mk(0, 1, 0.9), mk(0, 2, 0.2)];
        sort_members_for_encode(
            &mut members,
            &FeatureOrder::Column {
                name: "level".to_string(),
                descending: false,
            },
        );
        let keys: Vec<u64> = members.iter().map(|m| m.key).collect();
        assert_eq!(keys, vec![0, 0, 1], "tiles must stay grouped and ascending");
        assert_eq!(
            members.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![2, 1, 0],
            "sorting applies within each tile, not across the whole level"
        );
    }

    /// Mixed numeric property types compare by numeric value, not by variant:
    /// a column read as Int in one batch and Double in another must not split
    /// into two ordering classes.
    #[test]
    fn numeric_order_keys_compare_across_property_variants() {
        let mk = |seq: u64, v: PropertyValue| Member {
            key: 0,
            seq,
            geom: Geometry::Point(Point::new(0.0, 0.0)).into(),
            props: SharedProps::new(vec![("n".into(), v)]),
            id: None,
            gap: GAP_NONE,
            band: 0,
        };
        let members = vec![
            mk(0, PropertyValue::Int(10)),
            mk(1, PropertyValue::Double(2.5)),
            mk(2, PropertyValue::UInt(7)),
        ];
        let order = FeatureOrder::Column {
            name: "n".to_string(),
            descending: false,
        };
        assert_eq!(paint_order(members, &order), vec![1, 2, 0]);
    }

    #[test]
    fn feature_order_parses_the_cli_spellings() {
        assert_eq!(
            "input".parse::<FeatureOrder>().unwrap(),
            FeatureOrder::Input
        );
        assert_eq!(
            "level".parse::<FeatureOrder>().unwrap(),
            FeatureOrder::Column {
                name: "level".to_string(),
                descending: false
            }
        );
        assert_eq!(
            "level:desc".parse::<FeatureOrder>().unwrap(),
            FeatureOrder::Column {
                name: "level".to_string(),
                descending: true
            }
        );
        assert_eq!(
            "level:asc".parse::<FeatureOrder>().unwrap(),
            FeatureOrder::Column {
                name: "level".to_string(),
                descending: false
            }
        );
        // An unknown direction is a typo, not a column named `level:dsc`.
        assert!("level:dsc".parse::<FeatureOrder>().is_err());
        assert!("".parse::<FeatureOrder>().is_err());
        assert!(":asc".parse::<FeatureOrder>().is_err());
    }

    /// #280 spatial fairness: on a point-dominated tile, vertex count carries no
    /// ranking signal, so the valve must keep a uniform stride across member
    /// (≈ Hilbert) order — not a first-N prefix that would clump survivors.
    #[test]
    fn drop_valve_point_tile_keeps_uniform_stride() {
        let members: Vec<Member> = (0..10).map(|i| point_member(i as f64, 0.0, i)).collect();
        let kept = select_kept_members(&members, 5);
        let xs: Vec<f64> = kept
            .iter()
            .map(|m| match &*m.geom {
                Geometry::Point(p) => p.x(),
                _ => unreachable!("built points"),
            })
            .collect();
        // Even stride 0,2,4,6,8 — spread across the tile — not the prefix 0..5.
        assert_eq!(xs, vec![0.0, 2.0, 4.0, 6.0, 8.0]);
    }

    /// Sized geometries keep the largest-by-vertex-count survivors (pre-#280
    /// behaviour, unchanged): the biggest features carry the tile's visual
    /// signal.
    #[test]
    fn drop_valve_sized_tile_keeps_largest() {
        let members: Vec<Member> = (1..=5)
            .map(|n| {
                let coords: Vec<(f64, f64)> = (0..n).map(|k| (k as f64, 0.0)).collect();
                Member {
                    key: 0,
                    seq: n as u64,
                    geom: Geometry::LineString(LineString::from(coords)).into(),
                    props: SharedProps::new(Vec::new()),
                    id: None,
                    gap: GAP_NONE,
                    band: 0,
                }
            })
            .collect();
        let kept = select_kept_members(&members, 2);
        let counts: Vec<usize> = kept.iter().map(|m| m.geom.coords_count()).collect();
        assert_eq!(counts, vec![5, 4]);
    }

    /// A `--feature-order` column the layer does not publish is a silent
    /// no-op — every key is `Missing`, so everything ties and the stable sort
    /// leaves input order, which is indistinguishable from success. A typo
    /// must therefore say something.
    #[test]
    fn absent_order_column_is_reported() {
        use arrow_schema::Field;

        let schema = Schema::new(vec![
            Field::new("elevation", DataType::Float64, true),
            Field::new("level", DataType::Int32, false),
            Field::new("geometry", DataType::Binary, false),
        ]);
        let column = |n: &str| FeatureOrder::Column {
            name: n.to_string(),
            descending: false,
        };

        let plain = PublishedNames::identity();
        assert!(!order_column_is_absent(
            &schema,
            &plain,
            &column("elevation")
        ));
        assert!(
            order_column_is_absent(&schema, &plain, &column("elevatoin")),
            "a misspelled column must be detectable"
        );
        // The default never warns.
        assert!(!order_column_is_absent(
            &schema,
            &plain,
            &FeatureOrder::Input
        ));

        // The check is against PUBLISHED names, not schema names: the reserved
        // `level` column is dropped from tile properties, so ordering by it
        // would silently do nothing.
        assert!(order_column_is_absent(&schema, &plain, &column("level")));
    }

    /// The two features have to agree on which name to sort by.
    ///
    /// #359 restores a source column that was renamed to clear the reserved
    /// `level`, so the tile advertises `level` while the schema still says
    /// `level_`. `--feature-order level` must therefore resolve — this is the
    /// flag's headline case (nested polygon bands), and checking the schema
    /// instead of the published names would reject exactly it.
    #[test]
    fn order_column_resolves_against_the_restored_source_name() {
        use arrow_schema::Field;

        let schema = Schema::new(vec![
            Field::new("level_", DataType::Float64, true),
            Field::new("level", DataType::Int32, false),
            Field::new("geometry", DataType::Binary, false),
        ]);
        let published = PublishedNames::from_renames(
            &BTreeMap::from([("level_".to_string(), "level".to_string())]),
            &schema,
            HashSet::new(),
        );
        let column = |n: &str| FeatureOrder::Column {
            name: n.to_string(),
            descending: false,
        };

        assert!(
            !order_column_is_absent(&schema, &published, &column("level")),
            "the restored source name is what the tile carries"
        );
        assert!(
            order_column_is_absent(&schema, &published, &column("level_")),
            "the internal name is not published, so sorting by it would no-op"
        );
    }

    /// The oversized-tile valve must not become the draw order.
    ///
    /// `select_kept_members` ranks sized geometry by vertex count to choose
    /// *which* features survive. That ranking is a selection signal; emitting
    /// the survivors in it silently overrides a pinned `--feature-order`, and
    /// it does so precisely on the dense tiles a caller pinned an order for.
    /// Here vertex count runs with `level`, so "largest first" is the exact
    /// inverse of the requested ascending order and the two are
    /// distinguishable.
    #[test]
    fn oversized_valve_keeps_the_requested_feature_order() {
        let members: Vec<Member> = (1..=5u64)
            .map(|i| {
                let coords: Vec<(f64, f64)> = (0..i * 10)
                    .map(|v| (v as f64 * 1e-6, v as f64 * 1e-6))
                    .collect();
                Member {
                    key: 0,
                    seq: i,
                    geom: Geometry::LineString(coords.into_iter().collect()).into(),
                    props: SharedProps::new(vec![(
                        "level".into(),
                        PropertyValue::Double(i as f64),
                    )]),
                    id: None,
                    gap: GAP_NONE,
                    band: 0,
                }
            })
            .collect();

        let ordered = ExportOptions {
            feature_order: FeatureOrder::Column {
                name: "level".to_string(),
                descending: false,
            },
            ..ExportOptions::default()
        };
        let kept = shed_to_fit(&members, 3, &ordered);
        let levels: Vec<u64> = kept.iter().map(|m| m.seq).collect();
        assert_eq!(
            levels,
            vec![3, 4, 5],
            "the three largest survive, but they must be painted in ascending \
             `level` order, not in the valve's largest-first ranking"
        );

        // Descending is the valve's own order here, so it must come back
        // reversed relative to the ascending case rather than untouched.
        let desc = ExportOptions {
            feature_order: FeatureOrder::Column {
                name: "level".to_string(),
                descending: true,
            },
            ..ExportOptions::default()
        };
        let kept: Vec<u64> = shed_to_fit(&members, 3, &desc)
            .iter()
            .map(|m| m.seq)
            .collect();
        assert_eq!(kept, vec![5, 4, 3]);

        // The default is unchanged: the valve's largest-first ranking stands,
        // which is the pre-#280 behaviour this must stay byte-identical to.
        let kept: Vec<u64> = shed_to_fit(&members, 3, &ExportOptions::default())
            .iter()
            .map(|m| m.seq)
            .collect();
        assert_eq!(kept, vec![5, 4, 3], "default order must not change");
    }

    /// `Some(0)` is the core off switch (CLI `--max-tile-size 0` / Python
    /// `tile_size_limit=0` map to it): the valve must never fire, no matter how
    /// dense the tile.
    #[test]
    fn size_limit_zero_disables_valve() {
        let members: Vec<Member> = (0..64)
            .map(|i| point_member(-100.0 + i as f64 * 0.001, 40.0, i))
            .collect();
        let tb = TileCoord::new(0, 0, 0).bounds();
        let opts = ExportOptions {
            tile_size_limit: Some(0),
            ..Default::default()
        };
        let mvt = encode_tile(
            &test_gzip_cache(),
            &members,
            &tb,
            &opts,
            &[],
            &mut FillTally::default(),
            &crate::diagnostics::Diagnostics::default(),
        );
        assert!(!mvt.oversized, "Some(0) must disable the cap");
        assert_eq!(mvt.features, members.len());
        assert_eq!(mvt.unencodable, 0);
        assert_eq!(mvt.quantized, 0);
    }
}
