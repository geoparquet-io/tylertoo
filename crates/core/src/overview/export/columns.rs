use super::*;

// ============================================================================
// Reading + property extraction
// ============================================================================

/// Test-support: the pre-partitioning in-memory reference path — build every
/// member for a fully materialized feature slice at `zoom` (unbounded key
/// range) and encode via the production member machinery.
///
/// #443: resolves `opts.feature_id` directly against `f.props` (there is no
/// Arrow schema on this path), stripping the named property out of the
/// member's tags exactly as the production Arrow path does -- so this
/// exercises the same "moved, not copied" contract with the same error types.
#[cfg(test)]
pub(super) fn test_gzip_cache() -> Arc<TileGzipCache> {
    Arc::new(TileGzipCache::new(8 * 1024 * 1024))
}

#[cfg(test)]
pub(super) fn encode_level_tiles(
    features: &[Feature],
    zoom: u8,
    opts: &ExportOptions,
) -> Result<Vec<EncodedTile>, ExportError> {
    let mut members = Vec::new();
    let mut gaps = RowGapCursor::new(vec![(0, features.len() as u64)]);
    for (fi, f) in features.iter().enumerate() {
        let seq = fi as u64;
        let id = match &opts.feature_id {
            None => None,
            Some(name) => Some(feature_id::feature_id_from_property(
                find_property_value(&f.props, name),
                name,
                zoom,
                seq,
            )?),
        };
        let props: SharedProps = SharedProps::new(
            f.props
                .iter()
                .filter(|(n, _)| opts.feature_id.as_ref() != Some(n))
                .map(|(n, v)| (Arc::from(n.as_str()), v.clone()))
                .collect(),
        );
        // #745: the slice stands in for one row group of one band.
        let (gap, band) = gaps.next_row(f.geom.bounding_rect());
        for (key, geom) in feature_tile_members(
            &f.geom,
            zoom,
            opts,
            0,
            u64::MAX,
            &crate::diagnostics::Diagnostics::default(),
        ) {
            members.push(Member {
                key,
                seq,
                geom: geom.into(),
                props: props.clone(),
                id,
                gap,
                band,
            });
        }
    }
    // `encode_members` returns gzip-COMPRESSED tile bytes (issue #227 moved
    // compression into the parallel encode section). This helper exists to
    // exercise MVT *encoding*, so decompress each tile back to raw MVT bytes;
    // the assertions in the tests below operate on the uncompressed payload.
    let mut tiles = encode_members(
        &test_gzip_cache(),
        members,
        zoom,
        opts,
        None,
        &ExportTimers::default(),
        &crate::diagnostics::Diagnostics::default(),
    )
    .expect("in-memory gzip is infallible");
    // #431: all-dropped carriers hold no tile bytes; the writer skips them too.
    tiles.retain(|t| t.feature_count > 0);
    for t in &mut tiles {
        t.data = crate::compression::decompress_capped(
            &t.data,
            Compression::Gzip,
            crate::compression::MAX_TILE_BYTES,
        )
        .expect("gzip roundtrip of just-compressed tile");
    }
    Ok(tiles)
}

/// Find a named property's value in an unindexed `(name, value)` list, for
/// the [`encode_level_tiles`] test-support path (no Arrow column to read).
#[cfg(test)]
pub(super) fn find_property_value<'a>(
    props: &'a [(String, PropertyValue)],
    name: &str,
) -> Option<&'a PropertyValue> {
    props.iter().find(|(n, _)| n == name).map(|(_, v)| v)
}

/// Test-support: read every feature (geometry + carried properties) of a level
/// band into memory.
#[cfg(test)]
pub(super) fn read_level_features(
    reader: &OverviewReader,
    level_idx: usize,
    crs: Crs,
) -> Result<Vec<Feature>, ExportError> {
    let batch_reader = reader.read_level(level_idx, None)?;
    let mut out = Vec::new();
    for batch in batch_reader {
        let batch = batch.map_err(|e| read_error(e.into()))?;
        decode_batch(&batch, geometry_name(reader), crs, &mut out)?;
    }
    Ok(out)
}

/// Test-support: decode one record batch into [`Feature`]s, excluding the
/// `level` column and any struct/list column (the bbox covering) from
/// properties.
#[cfg(test)]
pub(super) fn decode_batch(
    batch: &RecordBatch,
    geometry: Option<&str>,
    crs: Crs,
    out: &mut Vec<Feature>,
) -> Result<(), ExportError> {
    let schema = batch.schema();
    let geom_idx = batch_geometry_index(batch, geometry)?;
    let geom_field = schema.field(geom_idx).clone();

    // Decode geometries.
    let garr: Arc<dyn GeoArrowArray> =
        from_arrow_array(batch.column(geom_idx).as_ref(), &geom_field)
            .map_err(|e| crate::GeoParquetReadError::decode("geometry decode", e))?;
    let mut geoms: Vec<Geometry<f64>> = Vec::with_capacity(batch.num_rows());
    extract_geometries_from_array(
        garr.as_ref(),
        &mut geoms,
        &crate::diagnostics::Diagnostics::default(),
    )?;

    // Pre-extract every exportable property column once.
    let prop_cols = property_columns(&schema, geom_idx, &PublishedNames::identity());
    let mut extracted: Vec<(String, Vec<Option<PropertyValue>>)> =
        Vec::with_capacity(prop_cols.len());
    for &(idx, ref name) in &prop_cols {
        extracted.push((name.clone(), extract_property_column(batch.column(idx))));
    }

    for row in 0..batch.num_rows() {
        let mut geom = geoms[row].clone();
        if matches!(crs, Crs::Epsg3857) {
            geom = reproject_3857_to_4326(&geom);
        }
        let mut props = Vec::with_capacity(extracted.len());
        for (name, col) in &extracted {
            if let Some(v) = &col[row] {
                props.push((name.clone(), v.clone()));
            }
        }
        out.push(Feature { geom, props });
    }
    Ok(())
}

/// The index of the overview file's geometry column in the file's schema
/// ([`OverviewReader::schema`], which carries the footer's `geo` key).
///
/// Resolved exactly as convert resolves its input and as
/// [`crate::resolved_schema::ResolvedSchema::geometry_name`] resolves the footer for
/// pruning (#518): the `geo` metadata's `primary_column`, then the name
/// heuristic (`geometry`, else the first `geom*`). The overview writer pins
/// `primary_column` to convert's geometry column, so a column named `SHAPE`
/// (an Esri file geodatabase) resolves by name (#675). Choosing by `geoarrow.*`
/// extension type instead would pick the first of several geometry columns
/// in schema order, which is not necessarily the primary one.
#[cfg(test)]
pub(super) fn geometry_index(schema: &Schema) -> Option<usize> {
    crate::resolved_schema::ResolvedSchema::resolve(schema)
        .ok()?
        .geometry_index()
}

/// The other geometry column the name heuristic alone would pick, when it
/// differs from `geom_idx` ([`crate::resolved_schema::ResolvedSchema::geometry_index`]).
///
/// Overview files converted before #675 left `primary_column` to the
/// geoparquet crate, which names the alphabetically first geometry column,
/// so an input with `geom` and a `centroid` beside it was written declaring
/// `centroid`. Export used to take `geom` by name and now follows the footer;
/// only a re-convert fixes such a file, so the export says so.
#[cfg(test)]
pub(super) fn geometry_name_disagreement(schema: &Schema, geom_idx: usize) -> Option<&str> {
    crate::resolved_schema::geometry_name_disagreement(schema, geom_idx)
}

/// The name of the overview file's geometry column ([`crate::resolved_schema::ResolvedSchema::geometry_index`]).
pub(super) fn geometry_name(reader: &OverviewReader) -> Option<&str> {
    reader.resolved_schema().geometry_name()
}

/// The index of the geometry column `geometry` ([`geometry_name`]) in a batch
/// read from the overview file.
///
/// [`crate::resolved_schema::ResolvedSchema::geometry_index`] cannot resolve a batch: the Parquet reader builds each
/// batch's schema from its columns alone, so the file's `geo` key, and with
/// it `primary_column`, is gone, and the name heuristic would take an
/// attribute such as `geom_type` over a geometry column named `SHAPE`. Field
/// names survive in every batch, projected ones included.
pub(super) fn batch_geometry_index(
    batch: &RecordBatch,
    geometry: Option<&str>,
) -> Result<usize, ExportError> {
    geometry
        .and_then(|name| batch.schema().index_of(name).ok())
        .ok_or(ExportError::NoGeometryColumn { path: None })
}

/// The MVT property key each overview-file column is published under (#359).
///
/// A source column that collided with a reserved overview column was renamed
/// on the way into the overview GeoParquet (`level` → `level_`, #288), because
/// in *that* file the reserved column is authoritative and both must coexist.
/// The PMTiles export carries no `level` property — it is asserted out of MVT
/// — so by tile-writing time the source name is free again and the reason for
/// the move has lapsed. Publishing the internal name instead hands the caller
/// a property their data never had, and a style keyed on the real name paints
/// nothing, with no error to point at.
///
/// Restoration is conditional, not automatic: a rename is undone only when its
/// source name is not already published by another column. `point_count` and
/// `coalesced_count` *are* real MVT properties in the modes that append them,
/// unless withheld (#379), so a column moved aside from one of those keeps the
/// renamed name — merging two columns into one key would be worse than the
/// wrong name.
///
/// It also carries the *suppressed* columns (#379): the `coalesced_count`
/// provenance counter the converter appends whenever line coalescing is on
/// (the default), when it never left 1 because nothing was merged (a polygon
/// layer through the coalescer, say). A column that is 1 on every row is not
/// a property of the data; in the tiles it is schema noise a style editor
/// shows as a real attribute, and a tag pair per feature. The overview file
/// keeps the column — its validator relies on it — this only decides what
/// the MVT carries. Whether the counter ever exceeded 1 is read from
/// row-group statistics, so the check costs nothing and a file whose stats
/// are missing keeps the column (nothing is known). `point_count` is never
/// withheld: `--cluster` is an explicit opt-in whose documented contract is
/// the column (a style scales symbols by it), and a schema that depends on
/// whether anything happened to cluster would break such styles. Suppression
/// is decided *before* restoration, so a withheld counter's name is free for
/// a source column that was moved aside from it.
#[derive(Debug, Clone, Default)]
pub(super) struct PublishedNames {
    /// Schema field name → published key, for the columns that differ.
    /// Empty for the identity mapping, which is the overwhelmingly common
    /// case (nothing collided, or the file predates the provenance).
    pub(super) restored: HashMap<String, String>,
    /// Schema field names that are not exported at all (#379).
    pub(super) suppressed: HashSet<String>,
    /// The bbox covering column the file's `geo` metadata declares for the
    /// geometry column, when it declares one. A covering is never a
    /// property (see [`super::super::writer::is_covering_column`]); this is the declared spelling,
    /// alongside the default names and the shape rule.
    pub(super) covering: Option<String>,
    /// The geometry column's name ([`geometry_name`]), resolved once from the
    /// file schema for the batches read from it ([`batch_geometry_index`]).
    pub(super) geometry: Option<String>,
    pub(super) properties: Option<HashSet<usize>>,
    /// The canonical level's index and its own view of the names, which also
    /// withholds the `coalesced_count` counter: that level never coalesces,
    /// so the counter is 1 on every one of its rows by construction (spec
    /// §13.2). Decided from the footer alone, so every build (sharded or
    /// not, at any thread count) withholds it from the same tiles.
    pub(super) canonical: Option<(usize, Box<PublishedNames>)>,
}

impl PublishedNames {
    /// Every column keeps its schema name.
    #[cfg(test)]
    pub(super) fn identity() -> Self {
        Self::default()
    }

    /// Resolve publication names for `schema` from a file's `renamed_columns`
    /// provenance (output name → source name; see [`Generalization`]), with
    /// `suppressed` the schema columns the export withholds (#379).
    #[cfg(test)]
    pub(super) fn from_renames(
        renames: &BTreeMap<String, String>,
        schema: &Schema,
        suppressed: HashSet<String>,
    ) -> Self {
        let restored = crate::resolved_schema::publication_names(schema, renames, &suppressed);
        for (renamed, source) in &restored {
            log::info!(
                "[export] publishing overview column {renamed:?} under its \
                 source name {source:?} (it was renamed on convert to clear \
                 the reserved overview column, #288/#359)"
            );
        }
        Self {
            restored,
            suppressed,
            covering: None,
            geometry: None,
            properties: None,
            canonical: None,
        }
    }

    /// Build from an open overview file: suppression of a `coalesced_count`
    /// counter that never left 1 (#379), then the rename mapping from the
    /// footer — in that order, so a withheld counter's name counts as free.
    /// `point_count` is deliberately not considered (see the type doc).
    pub(super) fn from_reader(reader: &OverviewReader) -> Self {
        let meta = reader.meta();
        let mut suppressed = HashSet::new();
        let coalescing = meta
            .generalization
            .as_ref()
            .and_then(|g| g.coalescing.as_ref())
            // The footer names the column, but only the converter's own
            // counter is eligible: a tampered or foreign footer must not be
            // able to withhold a real data column.
            .filter(|c| c.coalesced_count_column == COALESCED_COUNT_COLUMN);
        if let Some(c) = coalescing {
            let column = c.coalesced_count_column.as_str();
            // #541 review: prefer the converter's own record of whether any
            // chain merged, which describes the whole conversion. The
            // row-group statistic only sees the levels this file holds, so a
            // zoom-capped coarse job (whose merges all happen finer than the
            // ceiling) would withhold a column the monolithic build publishes,
            // and the two builds' coarse tiles would differ. Files written
            // before the flag existed fall back to the statistic.
            let never_merged = match c.merged {
                Some(merged) => !merged,
                None => reader.int_column_max(column) == Some(1),
            };
            if never_merged {
                log::info!(
                    "[export] not exporting {column:?}: it is 1 on every row \
                     (no lines were merged), so it says nothing about the data; \
                     the overview file keeps it (#379)"
                );
                suppressed.insert(column.to_string());
            }
        }
        let descriptor = reader.resolved_schema();
        let names = Self {
            restored: descriptor.publication_names(&suppressed),
            suppressed,
            covering: descriptor
                .covering()
                .and_then(|c| c.xmin_path.first().cloned()),
            geometry: descriptor.geometry_name().map(str::to_owned),
            properties: Some(descriptor.properties().iter().copied().collect()),
            canonical: None,
        };
        names
    }

    /// Withhold the converter's `coalesced_count` from the canonical level's
    /// tiles, where it is 1 on every row (#379 scoped per level). The column
    /// stays at the coarser levels whenever any chain merged. A caller who
    /// names the counter in `--include-property` keeps it everywhere; a
    /// `--feature-order` on it is unaffected, since a key missing from every
    /// member of a tile ranks them all equal, as all-1 values did. A capped
    /// coarse job's file (`zoom_ceiling`) holds no canonical level, so it
    /// withholds nothing here.
    pub(super) fn withhold_counter_at_canonical(
        &mut self,
        meta: &OverviewsMeta,
        selection: &PropertySelection,
    ) {
        let column = COALESCED_COUNT_COLUMN;
        let coalesced = meta.generalization.as_ref().is_some_and(|g| {
            g.zoom_ceiling.is_none()
                && g.coalescing
                    .as_ref()
                    .is_some_and(|c| c.coalesced_count_column == column)
        });
        let Some(canonical) = meta.canonical_level.and_then(|c| usize::try_from(c).ok()) else {
            return;
        };
        let published = self.publish(column);
        let named = selection
            .include
            .as_ref()
            .is_some_and(|names| names.iter().any(|n| n == published));
        if !coalesced || named || self.is_suppressed(column) {
            return;
        }
        let mut view = self.clone();
        view.suppress(column);
        self.canonical = Some((canonical, Box::new(view)));
    }

    /// The names level `level_idx`'s features are published under.
    pub(super) fn at_level(&self, level_idx: usize) -> &Self {
        match &self.canonical {
            Some((canonical, view)) if *canonical == level_idx => view,
            _ => self,
        }
    }

    /// Apply a caller's property selection (#386) on top of the file-derived
    /// naming: every exportable property whose *published* name the
    /// selection drops is withheld. Names are matched as the tile would
    /// carry them, which is what a style author has in front of them, so a
    /// column restored from a reserved-column rename is addressed by its
    /// restored name. An `include` naming no exportable property is an
    /// error; excluding the `--feature-order` column is too, since the sort
    /// would then have nothing to read.
    ///
    /// An explicit `include` wins over the #379 heuristic: a counter the
    /// file-derived naming withheld because it never left 1 is exported
    /// after all when the caller names it, since the column is plainly in
    /// the file and asking for it is not a typo.
    pub(super) fn with_selection(
        mut self,
        schema: &Schema,
        geom_idx: usize,
        selection: &PropertySelection,
        feature_order: &FeatureOrder,
    ) -> Result<Self, ExportError> {
        if selection.is_identity() {
            return Ok(self);
        }
        if let Some(include) = &selection.include {
            let exported: Vec<String> = property_columns(schema, geom_idx, &self)
                .into_iter()
                .map(|(_, n)| n)
                .collect();
            for name in include {
                if exported.contains(name) {
                    continue;
                }
                // Suppression is keyed by schema name; the include list
                // speaks published names, so map through the rename table.
                let withheld = schema
                    .fields()
                    .iter()
                    .map(|f| f.name())
                    .find(|n| self.is_suppressed(n) && self.publish(n) == name)
                    .cloned();
                if let Some(schema_name) = withheld {
                    log::info!(
                        "[export] exporting {schema_name:?} after all: it was withheld \
                         (#379) but --include-property names it"
                    );
                    self.suppressed.remove(&schema_name);
                }
            }
        }
        let exportable: Vec<(usize, String)> = property_columns(schema, geom_idx, &self);
        let published: Vec<&str> = exportable.iter().map(|(_, n)| n.as_str()).collect();
        if let Some(include) = &selection.include {
            for name in include {
                if !published.contains(&name.as_str()) {
                    // #434: a column that IS in the file but has no MVT
                    // encoding gets its own message; "unknown" would send
                    // the caller looking for a typo that is not there.
                    // Scanned over the same candidates `property_columns`
                    // draws from, so the bbox covering struct (index
                    // metadata, never a property) falls through to its own
                    // check below rather than being called an unencodable
                    // struct.
                    if let Some((_, f)) = candidate_columns(schema, geom_idx, &self)
                        .find(|(_, f)| self.publish(f.name()) == name)
                    {
                        return Err(ExportError::UnsupportedProperty {
                            name: name.clone(),
                            data_type: format!("{:?}", f.data_type()),
                        });
                    }
                    // #401: the covering is in the file but is never a
                    // property; say so, as the convert side does.
                    let geom = schema.fields().get(geom_idx).map(|g| g.name().as_str());
                    if schema.fields().iter().any(|f| {
                        f.name() == name
                            && super::super::writer::is_covering_column(
                                f,
                                geom,
                                self.covering.as_deref(),
                            )
                    }) {
                        return Err(ExportError::CoveringProperty { name: name.clone() });
                    }
                    return Err(ExportError::UnknownProperty {
                        name: name.clone(),
                        available: published
                            .iter()
                            .map(|n| format!("{n:?}"))
                            .collect::<Vec<_>>()
                            .join(", "),
                    });
                }
            }
        }
        // Every column the selection can address, exportable or not (#434):
        // excluding a binary column the export would otherwise drop with a
        // warning is a real choice, not a no-op, and silences that warning.
        let candidates: Vec<(usize, String)> = candidate_columns(schema, geom_idx, &self)
            .map(|(i, f)| (i, self.publish(f.name()).to_string()))
            .collect();
        for name in &selection.exclude {
            if !candidates.iter().any(|(_, n)| n == name) {
                log::warn!(
                    "[export] excluded property {name:?} is not exported by this file anyway"
                );
            }
        }
        if let FeatureOrder::Column { name, .. } = feature_order {
            if published.contains(&name.as_str()) && !selection.keeps(name) {
                return Err(ExportError::PropertyRequiredByKnob {
                    name: name.clone(),
                    knob: "--feature-order".to_string(),
                });
            }
        }
        let mut dropped = 0usize;
        for (idx, name) in &candidates {
            if !selection.keeps(name) {
                self.suppressed.insert(schema.field(*idx).name().clone());
                if exportable.iter().any(|(i, _)| i == idx) {
                    dropped += 1;
                }
            }
        }
        let kept: Vec<&str> = published
            .iter()
            .copied()
            .filter(|n| selection.keeps(n))
            .collect();
        log::info!(
            "[export] property selection: keeping {} of {} ({}), dropping {dropped}",
            kept.len(),
            published.len(),
            if kept.is_empty() {
                "none — geometry only".to_string()
            } else {
                kept.iter()
                    .map(|n| format!("{n:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        Ok(self)
    }

    /// Whether a schema column is withheld from the tiles entirely.
    pub(super) fn is_suppressed(&self, schema_name: &str) -> bool {
        self.suppressed.contains(schema_name)
    }

    /// Withhold a schema column from the tiles entirely (#443: a
    /// `--feature-id` column is moved to the feature id and must never also
    /// publish as a regular property, so this is applied unconditionally
    /// after resolving it -- regardless of what the ordinary property
    /// selection already decided for that column).
    pub(super) fn suppress(&mut self, schema_name: &str) {
        self.suppressed.insert(schema_name.to_string());
    }

    /// The MVT key for a schema field name.
    pub(super) fn publish<'a>(&'a self, schema_name: &'a str) -> &'a str {
        self.restored
            .get(schema_name)
            .map_or(schema_name, String::as_str)
    }
}

/// The `(index, published name)` of every exportable property column:
/// everything that is not the geometry column, not the `level` column, not
/// the geometry's bbox covering struct, and whose type the export can encode
/// ([`is_exportable`]: MVT scalars directly, struct/list/map columns as JSON
/// strings). The index addresses the real schema column; the name is what the
/// tile advertises, which differs when a rename is restored ([`PublishedNames`]).
pub(super) fn property_columns(
    schema: &Schema,
    geom_idx: usize,
    published: &PublishedNames,
) -> Vec<(usize, String)> {
    candidate_columns(schema, geom_idx, published)
        .filter(|&(_, f)| is_exportable(f.data_type()))
        .map(|(i, f)| (i, published.publish(f.name()).to_string()))
        .collect()
}

/// The property columns the export drops because their type has no MVT
/// encoding (#434), in schema order. Everything [`property_columns`] would
/// consider and [`is_exportable`] rejects; a column the selection already
/// withheld is not "dropped", it was asked for, so it is not listed.
pub(super) fn skipped_property_columns(
    schema: &Schema,
    geom_idx: usize,
    published: &PublishedNames,
) -> Vec<SkippedPropertyColumn> {
    candidate_columns(schema, geom_idx, published)
        .filter(|&(_, f)| !is_exportable(f.data_type()))
        .map(|(_, f)| SkippedPropertyColumn {
            name: published.publish(f.name()).to_string(),
            data_type: format!("{:?}", f.data_type()),
        })
        .collect()
}

/// Every schema column that is a property candidate: not the geometry, not
/// the `level` column, not a bbox covering struct ([`super::super::writer::is_covering_column`]:
/// index metadata, not user data -- so it is neither exported nor warned
/// about), and not withheld by the selection.
pub(super) fn candidate_columns<'a>(
    schema: &'a Schema,
    geom_idx: usize,
    published: &'a PublishedNames,
) -> impl Iterator<Item = (usize, &'a arrow_schema::Field)> + 'a {
    let geom = schema.fields().get(geom_idx).map(|g| g.name().as_str());
    schema
        .fields()
        .iter()
        .enumerate()
        .filter(move |&(i, f)| {
            published.properties.as_ref().map_or(
                i != geom_idx
                    && !f.name().eq_ignore_ascii_case(LEVEL_COLUMN)
                    && !super::super::writer::is_covering_column(
                        f,
                        geom,
                        published.covering.as_deref(),
                    ),
                |indices| indices.contains(&i),
            ) && !published.is_suppressed(f.name())
        })
        .map(|(i, f)| (i, f.as_ref()))
}

/// The columns pass 2 attaches to each member: the exportable properties
/// ([`property_columns`]) and the resolved `--feature-id` column (#443).
pub(super) struct AttrColumns<'a> {
    pub(super) names: PropertyNames,
    /// `(column index, published name)` in schema order.
    pub(super) props: Vec<(usize, Arc<str>)>,
    pub(super) feature_id: Option<&'a ResolvedFeatureId>,
}

impl<'a> AttrColumns<'a> {
    pub(super) fn new(
        schema: &Schema,
        geom_idx: usize,
        published: &PublishedNames,
        feature_id: Option<&'a ResolvedFeatureId>,
    ) -> Self {
        let props: Vec<_> = property_columns(schema, geom_idx, published)
            .into_iter()
            .map(|(idx, name)| (idx, Arc::<str>::from(name)))
            .collect();
        let names: PropertyNames = props
            .iter()
            .map(|(_, n)| n.clone())
            .collect::<Vec<_>>()
            .into();
        Self {
            props,
            names,
            feature_id,
        }
    }

    /// Extract every column of one batch once, for per-row materialization.
    ///
    /// PERF (#803): the batch gets its own copy of the schema table. Cloning
    /// the process-wide `Arc` per row and member would make every pool thread
    /// bump one refcount cache line (~5% more cycles at equal instructions,
    /// #874). `SpillNames` dedupes copies by content. Values are written
    /// directly into rows.
    pub(super) fn extract(&self, batch: &RecordBatch) -> Result<BatchAttributes<'a>, ExportError> {
        Ok(BatchAttributes {
            names: self.names.iter().cloned().collect::<Vec<_>>().into(),
            rows: packed_props::arrow::extract(
                self.props
                    .iter()
                    .map(|(idx, _)| batch.column(*idx).as_ref()),
                batch.num_rows(),
            )?,
            ids: self.feature_id.map(|fid| {
                let ids = feature_id_values(batch.column(fid.idx))
                    .map(|v| v.into_iter().map(Result::ok).collect())
                    .unwrap_or_default();
                (fid, ids)
            }),
            #[cfg(debug_assertions)]
            read: vec![false; batch.num_rows()],
        })
    }
}

/// One batch's extracted [`AttrColumns`].
pub(super) struct BatchAttributes<'a> {
    pub(super) names: PropertyNames,
    pub(super) rows: Vec<packed_props::arrow::Row>,
    /// The `--feature-id` column and its per-row ids (`None` = invalid).
    pub(super) ids: Option<(&'a ResolvedFeatureId, Vec<Option<u64>>)>,
    /// Rows [`Self::row`] has already moved out (debug builds only).
    #[cfg(debug_assertions)]
    pub(super) read: Vec<bool>,
}

impl BatchAttributes<'_> {
    /// Row `row`'s `--feature-id`, if one is set.
    pub(super) fn id(&self, row: usize) -> Result<Option<u64>, ExportError> {
        Ok(match &self.ids {
            None => None,
            // `validate_feature_id_column` already vouched for every row of
            // the file, so this only fires if the file changed underneath
            // the export.
            Some((fid, ids)) => Some(ids.get(row).copied().flatten().ok_or_else(|| {
                ExportError::Arrow(arrow_schema::ArrowError::InvalidArgumentError(format!(
                    "--feature-id column {:?} holds an invalid value that the up-front check \
                     did not see; was the overview file modified during the export?",
                    fid.name
                )))
            })?),
        })
    }

    /// Row `row`'s properties, shared across its members via `Arc`.
    ///
    /// Moves the already-filled row buffer into immutable shared storage.
    /// Call once per row; debug builds catch a second call.
    pub(super) fn props(&mut self, row: usize) -> SharedProps {
        #[cfg(debug_assertions)]
        assert!(
            !std::mem::replace(&mut self.read[row], true),
            "BatchAttributes::props({row}) read twice: its values were already moved out"
        );
        #[cfg(test)]
        PROPS_BUILT.with(|n| n.set(n.get() + 1));
        std::mem::take(&mut self.rows[row]).finish(self.names.clone())
    }
}

#[cfg(test)]
thread_local! {
    /// Rows whose properties [`BatchAttributes::props`] built on this thread
    /// (test counter).
    pub(super) static PROPS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// MVT-encodable Arrow scalar types.
///
/// MVT's value union is string/float/double/int/uint/bool — it has no temporal
/// type and no decimal. That is a reason to *encode* those, not to drop them:
/// a dropped column is silent data loss, and a time column is usually the axis
/// the map is read along. Temporal types render as ISO 8601 strings (the
/// tippecanoe convention) and decimals as doubles; see
/// [`packed_props::arrow`].
pub(super) fn is_supported_scalar(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Utf8
            | DataType::LargeUtf8
            | DataType::Boolean
            | DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Float32
            | DataType::Float64
    ) || is_temporal_scalar(dt)
        || matches!(dt, DataType::Decimal128(_, _) | DataType::Decimal256(_, _))
}

/// Arrow types that reach the tile as a JSON string (#434): struct, list,
/// large list, fixed-size list and map columns. tippecanoe does the same for
/// nested GeoJSON attributes -- `stringify_value` in `read_json.cpp` turns
/// any object or array into an `mvt_string` holding its `json_stringify`
/// text -- and it is what the Overture `names` struct and `sources` list
/// need to survive the export at all. The nested leaves are rendered by
/// the direct Arrow writer's scalar rules; a leaf type without one
/// (binary, an unrenderable timezone) renders as JSON `null` rather than
/// dropping the whole column.
/// Whether a (possibly dictionary-encoded) type renders as a JSON string,
/// i.e. can key a JSON object.
pub(super) fn is_string_type(dt: &DataType) -> bool {
    match dt {
        DataType::Utf8 | DataType::LargeUtf8 => true,
        DataType::Dictionary(_, value) => is_string_type(value),
        _ => false,
    }
}

pub(super) fn is_json_encoded(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Struct(_)
            | DataType::List(_)
            | DataType::LargeList(_)
            | DataType::FixedSizeList(_, _)
            | DataType::Map(_, _)
    )
}

/// Whether a column of this type is exported as a property: an MVT scalar
/// ([`is_supported_scalar`]), a JSON-encoded nested type
/// ([`is_json_encoded`]), or a dictionary over either (unwrapped on
/// extraction). [`property_columns`], [`field_metadata`] and
/// the direct Arrow writer all gate on this one predicate, so a column
/// advertised in `vector_layers.fields` is always one the tiles can fill.
pub(super) fn is_exportable(dt: &DataType) -> bool {
    match dt {
        DataType::Dictionary(_, value) => is_exportable(value),
        dt => is_supported_scalar(dt) || is_json_encoded(dt),
    }
}

/// The `vector_layers.fields` type name for an exportable column, or `None`
/// for one the export drops.
pub(super) fn field_type_name(dt: &DataType) -> Option<&'static str> {
    match dt {
        DataType::Utf8 | DataType::LargeUtf8 => Some("String"),
        DataType::Boolean => Some("Boolean"),
        DataType::Dictionary(_, value) => field_type_name(value),
        // Temporal values reach the tile as ISO 8601 text and nested values
        // as JSON text, so the field must be advertised as what a client
        // will actually read.
        dt if is_temporal_scalar(dt) || is_json_encoded(dt) => Some("String"),
        dt if is_supported_scalar(dt) => Some("Number"),
        _ => None,
    }
}

/// Arrow types rendered as ISO 8601 strings rather than numbers.
///
/// Rendering is delegated to `arrow_cast`'s own formatter, so the text matches
/// what every other Arrow consumer shows for the same value instead of a
/// hand-rolled variant of it.
pub(super) fn is_temporal_scalar(dt: &DataType) -> bool {
    match dt {
        DataType::Date32 | DataType::Date64 | DataType::Time32(_) | DataType::Time64(_) => true,
        // A timezone is only supported if we can render it -- see
        // `utc_equivalent_tz`. Claiming otherwise would advertise the field in
        // `vector_layers.fields` and then emit null for every value, which is
        // worse than dropping the column, because the archive then promises a
        // field that is not on a single feature.
        DataType::Timestamp(_, tz) => tz.as_ref().is_none_or(|tz| utc_equivalent_tz(tz)),
        _ => false,
    }
}

/// Whether a timestamp's timezone is one we can render without a tz database.
///
/// `arrow-array` only understands named zones when its (implicit, undocumented)
/// `chrono-tz` feature is on, which pulls in the whole IANA database; without
/// it `ArrayFormatter::try_new` fails outright for anything but a numeric
/// offset. That matters because parquet's `TIMESTAMP` carries a single
/// `isAdjustedToUTC` bool, which the parquet crate maps to literally
/// `Some("UTC")` -- so the tz-aware case is the COMMON one, not an exotic one,
/// and dropping it would take the time axis with it.
///
/// Every timestamp reaching the export comes from an overview parquet file, so
/// the tz is always `None` or a UTC spelling. Those we normalise to the fixed
/// offset `+00:00`, which arrow renders identically (`...Z`) with no extra
/// dependency. A genuine named zone cannot arrive by that route; if one ever
/// does it is not advertised and not encoded, exactly as before this change.
pub(super) fn utc_equivalent_tz(tz: &str) -> bool {
    matches!(
        tz.trim(),
        "UTC" | "utc" | "Utc" | "Z" | "z" | "+00:00" | "-00:00" | "00:00"
    )
}

/// The datatype to format a temporal array as: a UTC-equivalent timestamp is
/// restated with a numeric offset so arrow will render it without `chrono-tz`.
pub(super) fn formattable_temporal_type(dt: &DataType) -> DataType {
    match dt {
        DataType::Timestamp(unit, Some(tz)) if utc_equivalent_tz(tz) => {
            DataType::Timestamp(*unit, Some("+00:00".into()))
        }
        other => other.clone(),
    }
}

/// Field-type metadata (`name -> "String"|"Number"|"Boolean"`) for the archive
/// `vector_layers.fields` block.
pub(super) fn field_metadata(
    schema: &Schema,
    geom_idx: Option<usize>,
    published: &PublishedNames,
) -> HashMap<String, String> {
    let geom_idx = geom_idx.unwrap_or(usize::MAX);
    candidate_columns(schema, geom_idx, published)
        .filter_map(|(_, f)| {
            field_type_name(f.data_type())
                .map(|ty| (published.publish(f.name()).to_string(), ty.to_string()))
        })
        .collect()
}

/// Extract one Arrow column into per-row optional [`PropertyValue`]s. Null cells
/// and unsupported types yield `None`.
#[cfg(test)]
pub(super) fn extract_property_column(col: &dyn arrow_array::Array) -> Vec<Option<PropertyValue>> {
    let n = col.len();
    macro_rules! prim {
        ($ty:ty, $variant:ident, $cast:ty) => {{
            let a = col.as_primitive::<$ty>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::$variant(a.value(i) as $cast))
                    }
                })
                .collect()
        }};
    }
    macro_rules! strcol {
        ($off:ty) => {{
            let a = col.as_string::<$off>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::String(a.value(i).to_string()))
                    }
                })
                .collect()
        }};
    }
    match col.data_type() {
        DataType::Utf8 => strcol!(i32),
        DataType::LargeUtf8 => strcol!(i64),
        DataType::Boolean => {
            let a = col.as_boolean();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        None
                    } else {
                        Some(PropertyValue::Bool(a.value(i)))
                    }
                })
                .collect()
        }
        DataType::Int8 => prim!(Int8Type, Int, i64),
        DataType::Int16 => prim!(Int16Type, Int, i64),
        DataType::Int32 => prim!(Int32Type, Int, i64),
        DataType::Int64 => prim!(Int64Type, Int, i64),
        DataType::UInt8 => prim!(UInt8Type, UInt, u64),
        DataType::UInt16 => prim!(UInt16Type, UInt, u64),
        DataType::UInt32 => prim!(UInt32Type, UInt, u64),
        DataType::UInt64 => prim!(UInt64Type, UInt, u64),
        DataType::Float32 => prim!(Float32Type, Float, f32),
        DataType::Float64 => prim!(Float64Type, Double, f64),
        // Decimals are numbers, so scale them out rather than drop the column.
        //
        // An unscaled decimal (scale <= 0) is an integer, and `DECIMAL(38,0)` is
        // a common carrier for IDs and parcel numbers -- routing one through f64
        // would silently round it past 53 bits, turning an ID into a different
        // ID. Those go out as an MVT int when they fit one. Everything else
        // becomes a double, the only MVT numeric wide enough; past 53 bits of
        // significand that loses precision (an `i128 as f64` rounds -- it is far
        // too small to reach infinity), which is the same loss every other f64
        // consumer takes and still beats dropping the value.
        DataType::Decimal128(_, scale) => {
            let a = col.as_primitive::<Decimal128Type>();
            let scale = i32::from(*scale);
            let f = 10f64.powi(scale);
            (0..n)
                .map(|i| {
                    (!a.is_null(i)).then(|| {
                        let raw = a.value(i);
                        let exact = (scale <= 0)
                            .then(|| {
                                10i128
                                    .checked_pow((-scale) as u32)
                                    .and_then(|m| raw.checked_mul(m))
                            })
                            .flatten()
                            .and_then(|v| i64::try_from(v).ok());
                        match exact {
                            Some(v) => PropertyValue::Int(v),
                            None => PropertyValue::Double(raw as f64 / f),
                        }
                    })
                })
                .collect()
        }
        // 256-bit decimals do not fit an i128 fast path, so let arrow render
        // the scaled text and parse that. Slower per value, but this is a rare
        // type and arrow already owns the scaling rules.
        DataType::Decimal256(_, _) => {
            let opts = FormatOptions::default();
            match ArrayFormatter::try_new(col, &opts) {
                Ok(fmt) => (0..n)
                    .map(|i| {
                        // Arrow's decimal display is always `-?\d+(\.\d+)?`, so
                        // the parse does not fail in practice. If it ever did,
                        // drop the value rather than manufacture a NaN: a NaN
                        // encodes into the tile fine but renders any downstream
                        // tile-to-GeoJSON step invalid.
                        (!col.is_null(i))
                            .then(|| fmt.value(i).to_string().parse().ok())
                            .flatten()
                            .map(PropertyValue::Double)
                    })
                    .collect(),
                Err(_) => vec![None; n],
            }
        }
        // MVT has no temporal type. Render with arrow_cast's own formatter so
        // the text matches what every other Arrow consumer shows, instead of
        // dropping the column and taking the time axis with it.
        //
        // DIVERGENCE FROM TIPPECANOE: tippecanoe has no opinion here -- it
        // passes through whatever text its input already held, which for a
        // DuckDB-written parquet is `2026-09-14 12:34:56`. We emit arrow's
        // rendering, `2026-09-14T12:34:56` (or `...Z` when the column is
        // UTC-stamped): ISO 8601, unambiguous about the separator, and
        // identical to what `tylertoo decode` and every other Arrow reader will
        // show for the same value. Only the tz-stamped form carries an offset,
        // so only that one is also RFC 3339. Matching the space form would mean
        // re-implementing arrow's formatter to be less correct.
        //
        // `Date32` renders as `2026-09-14` and `Date64` as `2026-09-14T00:00:00`
        // -- arrow's own distinction, kept deliberately. Truncating `Date64`
        // would discard a time component the type is allowed to carry, and
        // parquet's DATE always decodes to `Date32`, so the split is unreachable
        // from the tiling path anyway.
        dt if is_temporal_scalar(dt) => {
            // Nulls are filtered below, so `FormatOptions`' null rendering is
            // never consulted.
            let opts = FormatOptions::default();
            let retyped: Option<arrow_array::ArrayRef> = match formattable_temporal_type(dt) {
                t if t == *dt => None,
                t => col
                    .to_data()
                    .into_builder()
                    .data_type(t)
                    .build()
                    .ok()
                    .map(arrow_array::make_array),
            };
            let target: &dyn Array = retyped.as_deref().unwrap_or(col);
            let out = match ArrayFormatter::try_new(target, &opts) {
                Ok(fmt) => (0..n)
                    .map(|i| {
                        (!col.is_null(i)).then(|| PropertyValue::String(fmt.value(i).to_string()))
                    })
                    .collect(),
                // Unreachable: `is_temporal_scalar` only admits types arrow can
                // format, and `field_metadata` gates on the same predicate, so
                // an un-formattable column is never selected or advertised.
                Err(e) => {
                    log::warn!("cannot format {dt:?} column as text, dropping it: {e}");
                    vec![None; n]
                }
            };
            out
        }
        // A dictionary is an encoding, not a type: unwrap it to its value
        // type (one `arrow_cast` pass over the batch, cheap) and extract that.
        // `is_exportable` admits a dictionary only over an exportable value
        // type, so the cast cannot land on the fallthrough below.
        DataType::Dictionary(_, value) => match arrow_cast::cast(col, value) {
            Ok(flat) => extract_property_column(flat.as_ref()),
            Err(e) => {
                log::warn!(
                    "cannot unwrap {:?} column, dropping it: {e}",
                    col.data_type()
                );
                vec![None; n]
            }
        },
        // #434: nested columns as JSON strings, the tippecanoe convention
        // (see `is_json_encoded`). One string per non-null row; a null row
        // stays null (no property), as for every scalar column.
        dt if is_json_encoded(dt) => json_property_column(col),
        _ => vec![None; n],
    }
}

// ============================================================================
// Nested properties as JSON (#434)
// ============================================================================

/// Render every row of a struct/list/map column as a JSON string.
///
/// The column is first resolved into a [`JsonNode`] tree -- one node per
/// nested Arrow array, with every scalar leaf extracted ONCE for the whole
/// batch through [`extract_property_column`] -- and then each row is written
/// straight into one reusable byte buffer. So the per-row cost is the write
/// itself: no intermediate `serde_json::Value` tree, no per-value
/// formatter, and exactly one `String` allocation per non-null row.
#[cfg(test)]
pub(super) fn json_property_column(col: &dyn Array) -> Vec<Option<PropertyValue>> {
    let node = JsonNode::resolve(col);
    let mut buf = Vec::with_capacity(64);
    (0..col.len())
        .map(|i| {
            if col.is_null(i) {
                return None;
            }
            buf.clear();
            node.write(&mut buf, i);
            // Only `serde_json` and ASCII punctuation ever wrote into `buf`,
            // so it is valid UTF-8; the checked conversion is belt and braces.
            String::from_utf8(buf.clone())
                .ok()
                .map(PropertyValue::String)
        })
        .collect()
}

/// A list or map column's value offsets rebased to its first row, and the
/// span of its child values its rows address. A sliced column (#740's
/// per-chunk extraction) keeps its whole child array, so resolving only the
/// span keeps a chunk's work proportional to its own rows.
#[cfg(test)]
pub(super) fn rebased_offsets<O: arrow_array::OffsetSizeTrait>(
    offsets: &[O],
) -> (Vec<usize>, std::ops::Range<usize>) {
    let first = offsets.first().map_or(0, |o| o.as_usize());
    let last = offsets.last().map_or(first, |o| o.as_usize());
    (
        offsets.iter().map(|o| o.as_usize() - first).collect(),
        first..last,
    )
}

/// One nested Arrow array, ready to be written row by row as JSON.
#[cfg(test)]
pub(super) enum JsonNode {
    /// A scalar leaf, already extracted for the whole array. An unsupported
    /// leaf type extracts to all-`None`, which renders as `null` per value.
    Leaf(Vec<Option<PropertyValue>>),
    /// A struct: its field names and one node per child, indexed by the
    /// same row.
    Struct {
        array: arrow_array::StructArray,
        fields: Vec<(String, JsonNode)>,
    },
    /// A variable-length list: `offsets[i]..offsets[i + 1]` of `values`.
    List {
        offsets: Vec<usize>,
        array: arrow_array::ArrayRef,
        values: Box<JsonNode>,
    },
    /// A map: `offsets[i]..offsets[i + 1]` of `(key, value)` pairs. A
    /// string key TYPE makes a JSON object; any other key type makes an
    /// array of two-element arrays, since JSON object keys can only be
    /// strings. Decided once per column from the type (`string_keys`), so
    /// every row and every batch agree on the shape.
    Map {
        offsets: Vec<usize>,
        array: arrow_array::ArrayRef,
        string_keys: bool,
        keys: Box<JsonNode>,
        values: Box<JsonNode>,
    },
}

#[cfg(test)]
impl JsonNode {
    pub(super) fn resolve(col: &dyn Array) -> Self {
        match col.data_type() {
            DataType::Struct(_) => {
                let array = col.as_struct().clone();
                let fields = array
                    .fields()
                    .iter()
                    .zip(array.columns())
                    .map(|(f, child)| (f.name().clone(), JsonNode::resolve(child.as_ref())))
                    .collect();
                JsonNode::Struct { array, fields }
            }
            DataType::List(_) => {
                let a = col.as_list::<i32>();
                let (offsets, span) = rebased_offsets(a.value_offsets());
                JsonNode::List {
                    offsets,
                    array: arrow_array::make_array(col.to_data()),
                    values: Box::new(JsonNode::resolve(
                        a.values().slice(span.start, span.len()).as_ref(),
                    )),
                }
            }
            DataType::LargeList(_) => {
                let a = col.as_list::<i64>();
                let (offsets, span) = rebased_offsets(a.value_offsets());
                JsonNode::List {
                    offsets,
                    array: arrow_array::make_array(col.to_data()),
                    values: Box::new(JsonNode::resolve(
                        a.values().slice(span.start, span.len()).as_ref(),
                    )),
                }
            }
            DataType::FixedSizeList(_, _) => {
                let a = col.as_fixed_size_list();
                let width = usize::try_from(a.value_length()).unwrap_or(0);
                JsonNode::List {
                    offsets: (0..=a.len()).map(|i| i * width).collect(),
                    array: arrow_array::make_array(col.to_data()),
                    values: Box::new(JsonNode::resolve(a.values().as_ref())),
                }
            }
            DataType::Map(_, _) => {
                let a = col.as_map();
                let (offsets, span) = rebased_offsets(a.value_offsets());
                JsonNode::Map {
                    offsets,
                    array: arrow_array::make_array(col.to_data()),
                    // Decided once from the key TYPE, not from the values:
                    // every row of the batch (and every batch of the column)
                    // then takes the same shape, and a key type with no
                    // rendering (binary) cannot masquerade as strings.
                    string_keys: is_string_type(a.keys().data_type()),
                    keys: Box::new(JsonNode::resolve(
                        a.keys().slice(span.start, span.len()).as_ref(),
                    )),
                    values: Box::new(JsonNode::resolve(
                        a.values().slice(span.start, span.len()).as_ref(),
                    )),
                }
            }
            // Unwrapped HERE, not through the leaf's own dictionary arm: a
            // dictionary over a struct or list must resolve into a nested
            // node, or its values would be JSON-stringified as a column of
            // their own and then embedded as string literals (double-encoded).
            DataType::Dictionary(_, value) => match arrow_cast::cast(col, value) {
                Ok(flat) => JsonNode::resolve(flat.as_ref()),
                Err(_) => JsonNode::Leaf(vec![None; col.len()]),
            },
            _ => JsonNode::Leaf(extract_property_column(col)),
        }
    }

    /// Whether row `i` of this node is null (renders as `null`).
    pub(super) fn is_null(&self, i: usize) -> bool {
        match self {
            JsonNode::Leaf(values) => values.get(i).is_none_or(Option::is_none),
            JsonNode::Struct { array, .. } => array.is_null(i),
            JsonNode::List { array, .. } | JsonNode::Map { array, .. } => array.is_null(i),
        }
    }

    /// Write row `i` as JSON. `serde_json` does every string escape and
    /// number rendering (a non-finite float becomes `null`, as `serde_json`
    /// renders it everywhere); only the punctuation is written by hand.
    pub(super) fn write(&self, out: &mut Vec<u8>, i: usize) {
        if self.is_null(i) {
            out.extend_from_slice(b"null");
            return;
        }
        match self {
            JsonNode::Leaf(values) => {
                // `is_null` above already returned for a `None`.
                let value = values[i].as_ref().expect("checked non-null");
                write_json_scalar(out, value);
            }
            JsonNode::Struct { fields, .. } => {
                out.push(b'{');
                for (k, (name, child)) in fields.iter().enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    write_json_str(out, name);
                    out.push(b':');
                    child.write(out, i);
                }
                out.push(b'}');
            }
            JsonNode::List {
                offsets, values, ..
            } => {
                out.push(b'[');
                for (k, j) in (offsets[i]..offsets[i + 1]).enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    values.write(out, j);
                }
                out.push(b']');
            }
            JsonNode::Map {
                offsets,
                string_keys,
                keys,
                values,
                ..
            } => {
                let string_keys = *string_keys;
                out.push(if string_keys { b'{' } else { b'[' });
                for (k, j) in (offsets[i]..offsets[i + 1]).enumerate() {
                    if k > 0 {
                        out.push(b',');
                    }
                    if string_keys {
                        // Arrow forbids null map keys; if one ever arrives,
                        // a quoted "null" keeps the object valid JSON.
                        if keys.is_null(j) {
                            write_json_str(out, "null");
                        } else {
                            keys.write(out, j);
                        }
                        out.push(b':');
                        values.write(out, j);
                    } else {
                        out.push(b'[');
                        keys.write(out, j);
                        out.push(b',');
                        values.write(out, j);
                        out.push(b']');
                    }
                }
                out.push(if string_keys { b'}' } else { b']' });
            }
        }
    }
}

/// Write one scalar as JSON.
#[cfg(test)]
pub(super) fn write_json_scalar(out: &mut Vec<u8>, value: &PropertyValue) {
    // `serde_json::to_writer` into a `Vec` cannot fail: the writer is
    // infallible and every variant here is serializable.
    let _ = match value {
        PropertyValue::String(s) => serde_json::to_writer(&mut *out, s),
        PropertyValue::Bool(b) => serde_json::to_writer(&mut *out, b),
        PropertyValue::Int(v) => serde_json::to_writer(&mut *out, v),
        PropertyValue::UInt(v) => serde_json::to_writer(&mut *out, v),
        PropertyValue::Float(v) => serde_json::to_writer(&mut *out, v),
        PropertyValue::Double(v) => serde_json::to_writer(&mut *out, v),
    };
}

/// Write one string as a JSON string literal.
pub(super) fn write_json_str(out: &mut Vec<u8>, s: &str) {
    let _ = serde_json::to_writer(&mut *out, s);
}

// ============================================================================
// CRS
// ============================================================================

/// Detect the overview file's CRS. A null/absent CRS is GeoParquet's default
/// EPSG:4326. EPSG:3857 is accepted (reprojected on read); anything else errors.
///
/// Classification is [`crate::quality::classify_crs_info`], the single shared
/// classifier — NOT a local substring match (#519). The hand-rolled
/// `identifier.contains("3857")` this replaces never saw a PROJJSON blob
/// whose `id` is structured rather than spelled out in the string, which is
/// exactly what an overview converted from a Web Mercator input now declares.
#[cfg(test)]
pub(super) fn detect_crs(
    path: &Path,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Crs, ExportError> {
    let file = File::open(path).map_err(crate::Error::Io)?;
    let metadata = tylertoo_io::read_parquet_metadata(&file).map_err(|e| {
        crate::Error::from(crate::GeoParquetReadError::Schema {
            message: e.to_string(),
        })
    })?;
    let descriptor = crate::resolved_schema::ResolvedSchema::from_metadata(&metadata)?;
    crs_from_descriptor(&descriptor, path, diagnostics)
}

pub(super) fn crs_from_descriptor(
    descriptor: &crate::resolved_schema::ResolvedSchema,
    path: &Path,
    diagnostics: &crate::diagnostics::Diagnostics,
) -> Result<Crs, ExportError> {
    use crate::quality::CrsKind;
    descriptor.record_diagnostics(diagnostics);
    let info = descriptor.crs_info().clone();
    match crate::quality::classify_crs_info(&info) {
        CrsKind::Wgs84 => Ok(Crs::Epsg4326),
        CrsKind::WebMercator => Ok(Crs::Epsg3857),
        // A null CRS (identifier None, name None) is the GeoParquet default
        // 4326 — the long-standing export default, kept deliberately.
        CrsKind::Unknown if info.identifier.is_none() && info.name.is_none() => Ok(Crs::Epsg4326),
        CrsKind::Unknown => Err(ExportError::UnsupportedCrs {
            crs: info
                .identifier
                .or(info.name)
                .unwrap_or_else(|| "unknown".to_string()),
            path: path.display().to_string(),
        }),
    }
}

/// Reproject one EPSG:3857 point (meters) to EPSG:4326 (lon/lat degrees).
#[inline]
pub(crate) fn webmerc_to_lnglat(x: f64, y: f64) -> (f64, f64) {
    let lng = x / WEBMERC_HALF_M * 180.0;
    let lat = (2.0 * (y / WEBMERC_HALF_M * std::f64::consts::PI).exp().atan()
        - std::f64::consts::FRAC_PI_2)
        .to_degrees();
    (lng, lat)
}

/// Reproject a geometry from EPSG:3857 (meters) to EPSG:4326 (lon/lat degrees)
/// so the geographic tile grid math applies.
pub(super) fn reproject_3857_to_4326(g: &Geometry<f64>) -> Geometry<f64> {
    g.map_coords(|c| {
        let (lng, lat) = webmerc_to_lnglat(c.x, c.y);
        geo::coord! { x: lng, y: lat }
    })
}

#[cfg(test)]
mod tests {
    use arrow_array::Int64Array;
    use arrow_schema::Field;
    use geo::Point;
    use geoarrow::{array::GeometryBuilder, datatypes::GeometryType};

    use super::super::tests::*;
    use super::*;

    // ------------------------------------------------------------------
    // CRS detection (#519)
    // ------------------------------------------------------------------

    /// Write a minimal GeoParquet file whose geometry column declares
    /// `crs_metadata`, so [`detect_crs`] has a real footer to read.
    fn write_crs_fixture(path: &Path, md: Option<geoarrow::datatypes::Metadata>) {
        use geoparquet::writer::{
            GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
        };
        use tylertoo_io::test_support::ArrowWriter;

        let geoms = [Geometry::Point(Point::new(0.0, 0.0))];
        let arr = match md {
            Some(md) => {
                let mut b =
                    GeometryBuilder::new(GeometryType::new(Arc::new(md))).with_prefer_multi(false);
                b.extend_from_iter(geoms.iter().map(Some));
                b.finish()
            }
            None => build_geometry_array(&geoms),
        };
        let schema = Arc::new(Schema::new(vec![
            Arc::new(Field::new("id", DataType::Int64, false)),
            Arc::new(arr.data_type().to_field("geometry", true)),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![0i64])), arr.to_array_ref()],
        )
        .unwrap();
        let opts = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .build();
        let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &opts).unwrap();
        let target = encoder.target_schema();
        let mut w =
            ArrowWriter::try_new(std::fs::File::create(path).unwrap(), target, None).unwrap();
        w.write(&encoder.encode_record_batch(&batch).unwrap())
            .unwrap();
        w.append_key_value_metadata(encoder.into_keyvalue().unwrap());
        w.close().unwrap();
    }

    /// #519 S3-A: `detect_crs` used to run its own `identifier.contains("3857")`
    /// substring match, which is blind to a PROJJSON `crs` whose EPSG code
    /// lives in a structured `id` — precisely what an overview converted from
    /// a Web Mercator input now declares. It must go through the shared
    /// [`crate::quality::classify_crs_info`] classifier instead.
    #[test]
    fn detect_crs_classifies_pseudo_mercator_projjson() {
        let projjson = serde_json::json!({
            "type": "ProjectedCRS",
            "name": "WGS 84 / Pseudo-Mercator",
            "id": { "authority": "EPSG", "code": 3857 }
        });
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_crs_fixture(
            tmp.path(),
            Some(geoarrow::datatypes::Metadata::new(
                geoarrow::datatypes::Crs::from_projjson(projjson),
                None,
            )),
        );
        assert_eq!(
            detect_crs(tmp.path(), &crate::diagnostics::Diagnostics::default()).unwrap(),
            Crs::Epsg3857
        );

        // The case the substring matcher could not see at all: PROJJSON with
        // NO structured id, so the identifier falls back to the *name* — no
        // "3857" anywhere in the string.
        let no_id = tempfile::NamedTempFile::new().unwrap();
        write_crs_fixture(
            no_id.path(),
            Some(geoarrow::datatypes::Metadata::new(
                geoarrow::datatypes::Crs::from_projjson(serde_json::json!({
                    "type": "ProjectedCRS",
                    "name": "WGS 84 / Pseudo-Mercator"
                })),
                None,
            )),
        );
        assert_eq!(
            detect_crs(no_id.path(), &crate::diagnostics::Diagnostics::default()).unwrap(),
            Crs::Epsg3857,
            "a Pseudo-Mercator PROJJSON with no id is still Web Mercator"
        );

        // ESRI's own Web Mercator code (#519 S4), as PROJJSON — the only
        // representation `geoparquet`'s default CRS transform writes.
        let esri = tempfile::NamedTempFile::new().unwrap();
        write_crs_fixture(
            esri.path(),
            Some(geoarrow::datatypes::Metadata::new(
                geoarrow::datatypes::Crs::from_projjson(serde_json::json!({
                    "type": "ProjectedCRS",
                    "name": "WGS_1984_Web_Mercator_Auxiliary_Sphere",
                    "id": { "authority": "ESRI", "code": 102100 }
                })),
                None,
            )),
        );
        assert_eq!(
            detect_crs(esri.path(), &crate::diagnostics::Diagnostics::default()).unwrap(),
            Crs::Epsg3857
        );

        // 4326 PROJJSON is still lon/lat...
        let tmp4326 = tempfile::NamedTempFile::new().unwrap();
        write_crs_fixture(
            tmp4326.path(),
            Some(geoarrow::datatypes::Metadata::new(
                geoarrow::datatypes::Crs::from_projjson(serde_json::json!({
                    "type": "GeographicCRS",
                    "name": "WGS 84",
                    "id": { "authority": "EPSG", "code": 4326 }
                })),
                None,
            )),
        );
        assert_eq!(
            detect_crs(tmp4326.path(), &crate::diagnostics::Diagnostics::default()).unwrap(),
            Crs::Epsg4326
        );

        // ...a file declaring no CRS at all is the GeoParquet default 4326...
        let bare = tempfile::NamedTempFile::new().unwrap();
        write_crs_fixture(bare.path(), None);
        assert_eq!(
            detect_crs(bare.path(), &crate::diagnostics::Diagnostics::default()).unwrap(),
            Crs::Epsg4326
        );

        // ...and a genuinely unsupported CRS is still a loud error.
        let utm = tempfile::NamedTempFile::new().unwrap();
        write_crs_fixture(
            utm.path(),
            Some(geoarrow::datatypes::Metadata::new(
                geoarrow::datatypes::Crs::from_projjson(serde_json::json!({
                    "type": "ProjectedCRS",
                    "name": "WGS 84 / UTM zone 33N",
                    "id": { "authority": "EPSG", "code": 32633 }
                })),
                None,
            )),
        );
        let err = detect_crs(utm.path(), &crate::diagnostics::Diagnostics::default()).unwrap_err();
        assert!(matches!(err, ExportError::UnsupportedCrs { .. }));
        // #452: and names the file.
        let msg = err.to_string();
        assert!(msg.contains(&utm.path().display().to_string()), "{msg}");
    }
}
