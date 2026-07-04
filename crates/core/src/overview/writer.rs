//! Level-banded GeoParquet overview writer (spec §4, §6.1).
//!
//! [`OverviewWriter`] wraps a [`parquet::arrow::ArrowWriter`] and a
//! [`geoparquet::writer::GeoParquetRecordBatchEncoder`]. The caller drives it
//! **coarse → fine**, one level at a time; the writer:
//!
//! 1. appends a NOT NULL `Int32` `level` column set to the level index to every
//!    batch (§4.1),
//! 2. writes the batches and forces a row-group boundary at the end of each
//!    level via [`ArrowWriter::flush`] (§4.2), recording the level's
//!    `row_group_end`,
//! 3. on [`OverviewWriter::finish`], writes the GeoParquet 1.1 `geo` metadata
//!    (with bbox covering, §4.4) plus the `geo:overviews` footer key (§3), and
//!    optionally the COGP-compatibility key (§3.1).
//!
//! The writer does **not** sort, thin, or simplify: it trusts the caller to
//! feed correctly ordered, already-generalized per-level batches (P1/P2).

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{Int32Array, RecordBatch};
use arrow_schema::extension::EXTENSION_TYPE_NAME_KEY;
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::metadata::KeyValue;
use parquet::file::properties::{EnabledStatistics, WriterProperties};
use parquet::schema::types::ColumnPath;

use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};

use super::level::{
    Generalization, Level, Mode, OverviewValidationError, OverviewsMeta, COGP_KEY, OVERVIEWS_KEY,
    SPEC_VERSION,
};

/// Name of the mandatory level column (§4.1).
pub const LEVEL_COLUMN: &str = "level";

/// Default ZSTD compression level (spec §4.5 recommends ZSTD).
pub const DEFAULT_ZSTD_LEVEL: i32 = 3;

/// Default maximum row-group size in rows (§4.5, configurable).
pub const DEFAULT_MAX_ROW_GROUP_SIZE: usize = 10_000;

/// Per-level specification supplied by the caller (coarse → fine).
#[derive(Debug, Clone)]
pub struct LevelSpec {
    /// Ground sample distance in meters (> 0), strictly decreasing across levels.
    pub gsd: f64,
    /// OPTIONAL Web Mercator zoom for this level (§5.2).
    pub zoom: Option<u8>,
}

impl LevelSpec {
    /// Convenience constructor.
    pub fn new(gsd: f64, zoom: Option<u8>) -> Self {
        Self { gsd, zoom }
    }
}

/// How the per-level row-group cap is derived from `max_row_group_size`
/// (issue #202).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RowGroupSizePolicy {
    /// Every level uses `max_row_group_size` as its cap (the shipped default).
    #[default]
    Constant,
    /// The cap **doubles per zoom step below the finest level's zoom**:
    /// `cap(level) = max_row_group_size << (finest_zoom − level_zoom)`,
    /// saturating. Rationale: a viewport rendered at zoom `z` covers ~4× the
    /// ground area of one at `z+1`, so the coarser a level is, the larger the
    /// slice of its band any real viewport reads anyway — bigger row groups
    /// there cost little pruning but cut request round trips. The finest
    /// (canonical) level keeps the exact `max_row_group_size` cap for tight
    /// bbox pruning. Levels without zoom metadata fall back to the base cap.
    ZoomScaled,
}

/// Configuration for an [`OverviewWriter`].
#[derive(Debug, Clone)]
pub struct OverviewWriterOptions {
    /// Level materialization mode (§2.2, §2.3).
    pub mode: Mode,
    /// Per-level GSD/zoom specs, ordered coarse → fine. One per level.
    pub levels: Vec<LevelSpec>,
    /// ZSTD compression level.
    pub zstd_level: i32,
    /// Row-group **cap** in rows (§4.5). Interpreted per level (H1): a level
    /// whose row count is `<= max_row_group_size` is written as a SINGLE row
    /// group; a larger level is split into `ceil(rows / max_row_group_size)`
    /// row groups of roughly uniform size. Small coarse bands therefore become
    /// one broad row group (read whole anyway) while fine bands keep tight
    /// per-row-group bbox statistics for pruning.
    pub max_row_group_size: usize,
    /// How the per-level cap is derived from `max_row_group_size` (#202).
    /// Default [`RowGroupSizePolicy::Constant`].
    pub row_group_size_policy: RowGroupSizePolicy,
    /// Keep full Parquet statistics on **every** column, including
    /// high-cardinality string/binary property columns and the WKB geometry
    /// column (H1). Default `false`: those columns' per-row-group min/max are
    /// suppressed because no reader uses them and they dominate the footer
    /// (Moldova: a 26-char ULID `id` × 167 row groups ⇒ 8.84 MB footer). The
    /// bbox covering and `level` column always keep their stats (the pruning
    /// index, §4.4). Set `true` for clients that push property predicates to the
    /// remote file and want row-group skipping on those columns.
    pub full_column_stats: bool,
    /// Emit the optional COGP-compatibility footer key (§3.1). Default `false`.
    pub cogp_compat_key: bool,
    /// Spec version string written to the footer.
    pub version: String,
    /// OPTIONAL generalization provenance (§3.5).
    pub generalization: Option<Generalization>,
}

impl OverviewWriterOptions {
    /// Construct options for the given mode and per-level specs, with defaults
    /// for compression / row-group size / flags.
    pub fn new(mode: Mode, levels: Vec<LevelSpec>) -> Self {
        Self {
            mode,
            levels,
            zstd_level: DEFAULT_ZSTD_LEVEL,
            max_row_group_size: DEFAULT_MAX_ROW_GROUP_SIZE,
            row_group_size_policy: RowGroupSizePolicy::default(),
            full_column_stats: false,
            cogp_compat_key: false,
            version: SPEC_VERSION.to_string(),
            generalization: None,
        }
    }
}

/// Errors produced by [`OverviewWriter`].
#[derive(Debug, thiserror::Error)]
pub enum WriterError {
    /// Underlying parquet error.
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// Arrow error (e.g. building a record batch).
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// JSON serialization error for the footer metadata.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// The produced footer metadata failed structural validation.
    #[error("overview metadata validation failed: {0}")]
    Validation(#[from] OverviewValidationError),
    /// The geoparquet encoder failed (metadata / covering generation).
    #[error("geoparquet encoder error: {0}")]
    GeoParquet(String),
    /// The source schema already contains a `level` column.
    #[error(
        "source schema already contains a '{LEVEL_COLUMN}' column \
         (matched case-insensitively; rename the source column first)"
    )]
    LevelColumnExists,
    /// The source schema has no geometry column (no geoarrow-extension field).
    #[error("source schema has no geometry column")]
    NoGeometryColumn,
    /// `write_level` was called out of order (must be 0, 1, 2, ... coarse→fine).
    #[error("write_level called out of order: expected level {expected}, got {got}")]
    LevelOutOfOrder {
        /// Expected next level index.
        expected: usize,
        /// Index the caller passed.
        got: usize,
    },
    /// Every declared level was empty: nothing was written, so no valid
    /// overview file can be produced (`levels` MUST be non-empty, §3.3).
    #[error(
        "all {expected} declared level(s) were empty — no output rows at any \
         level (empty input, or every feature dropped at every scale)"
    )]
    AllLevelsEmpty {
        /// Levels declared in the options.
        expected: usize,
    },
    /// `finish` was called before all declared levels were written.
    #[error("finish called with {written} of {expected} levels written")]
    IncompleteLevels {
        /// Levels written so far.
        written: usize,
        /// Levels declared in the options.
        expected: usize,
    },
}

/// Outcome of one [`OverviewWriter::write_level`] call.
///
/// A level whose batch stream yields zero rows is **omitted** from the output
/// (and later levels are renumbered) rather than treated as an error: spec
/// §7.3 forbids empty levels and requires the writer to omit-and-renumber.
/// Callers use the outcome to keep their own level bookkeeping (reports,
/// warnings) aligned with the physical file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "an empty level is silently omitted; callers must renumber their own bookkeeping"]
pub enum LevelWriteOutcome {
    /// The level produced rows and was written (at least one row group).
    Written,
    /// The level produced no rows: nothing was written, the declared spec is
    /// dropped from the footer, and subsequent levels shift down by one
    /// physical index (§7.3).
    SkippedEmpty,
}

/// A level-banded GeoParquet overview writer.
pub struct OverviewWriter<W: Write + Send> {
    writer: ArrowWriter<W>,
    encoder: GeoParquetRecordBatchEncoder,
    /// Source schema augmented with the trailing `level` column (the schema the
    /// batches passed to [`GeoParquetRecordBatchEncoder::encode_record_batch`]
    /// must have).
    augmented_schema: SchemaRef,
    options: OverviewWriterOptions,
    /// Source-batch column indices to drop before encoding: pre-existing
    /// bbox-covering struct columns whose name collides with the covering the
    /// encoder will generate (§4.4). See [`Self::try_new`].
    drop_indices: Vec<usize>,
    /// `row_group_end` recorded for each completed (non-empty) level.
    level_row_group_ends: Vec<i64>,
    /// For each completed level, the index of its declared
    /// [`LevelSpec`](OverviewWriterOptions::levels). Diverges from the
    /// physical index once an empty level is skipped (§7.3).
    written_spec_indices: Vec<usize>,
    /// Index of the next level expected by [`Self::write_level`].
    next_level_idx: usize,
}

impl OverviewWriter<File> {
    /// Create an overview writer that writes to `path`.
    pub fn create<P: AsRef<Path>>(
        path: P,
        source_schema: &Schema,
        options: OverviewWriterOptions,
    ) -> Result<Self, WriterError> {
        let file = File::create(path)?;
        Self::try_new(file, source_schema, options)
    }
}

impl<W: Write + Send> OverviewWriter<W> {
    /// Create an overview writer over an arbitrary sink.
    ///
    /// `source_schema` is the schema of the input table (including its geometry
    /// column, carrying the GeoArrow extension metadata). It MUST NOT already
    /// contain a `level` column.
    pub fn try_new(
        sink: W,
        source_schema: &Schema,
        options: OverviewWriterOptions,
    ) -> Result<Self, WriterError> {
        // Case-insensitive: SQL engines (DuckDB) resolve identifiers
        // case-insensitively, so a source `LEVEL` column would silently
        // shadow ours in `WHERE level = k` (V1 finding F2).
        if source_schema
            .fields()
            .iter()
            .any(|f| f.name().eq_ignore_ascii_case(LEVEL_COLUMN))
        {
            return Err(WriterError::LevelColumnExists);
        }

        let geometry_columns = geometry_columns(source_schema);
        if geometry_columns.is_empty() {
            return Err(WriterError::NoGeometryColumn);
        }

        // Drop any pre-existing bbox-covering struct column whose name collides
        // with the covering the geoparquet encoder will *generate* for a
        // geometry column (§4.4). gpio-optimized inputs (the documented input
        // contract, §4.3) always carry such a `bbox` covering; passing it
        // through would (a) duplicate the column and (b) — because the `geo`
        // covering metadata resolves the name to the *first* physical match —
        // point the covering at the stale, pre-generalization input bbox
        // instead of the encoder's freshly computed one. We drop it so the
        // encoder's authoritative covering is the only one present.
        let covering_names: std::collections::HashSet<String> = geometry_columns
            .iter()
            .map(|g| covering_name_for(g))
            .collect();
        let drop_indices: Vec<usize> = source_schema
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| covering_names.contains(f.name()) && is_bbox_covering_struct(f))
            .map(|(i, _)| i)
            .collect();

        // Augment schema: retained source fields + NOT NULL Int32 `level`
        // column (§4.1).
        let mut fields: Vec<Arc<Field>> = source_schema
            .fields()
            .iter()
            .enumerate()
            .filter(|(i, _)| !drop_indices.contains(i))
            .map(|(_, f)| f.clone())
            .collect();
        fields.push(Arc::new(Field::new(LEVEL_COLUMN, DataType::Int32, false)));
        let augmented_schema = Arc::new(Schema::new_with_metadata(
            fields,
            source_schema.metadata().clone(),
        ));

        // GeoParquet writer options: WKB encoding (GeoParquet 1.0/1.1-safe) with
        // bbox covering generation (§4.4).
        let gpq_options = GeoParquetWriterOptionsBuilder::default()
            .set_encoding(GeoParquetWriterEncoding::WKB)
            .set_generate_covering(true)
            .build();

        let encoder = GeoParquetRecordBatchEncoder::try_new(&augmented_schema, &gpq_options)
            .map_err(|e| WriterError::GeoParquet(e.to_string()))?;
        let target_schema = encoder.target_schema();

        let props = build_writer_properties(&options, &geometry_columns, &augmented_schema)?;
        let writer = ArrowWriter::try_new(sink, target_schema, Some(props))?;

        Ok(Self {
            writer,
            encoder,
            augmented_schema,
            options,
            drop_indices,
            level_row_group_ends: Vec::new(),
            written_spec_indices: Vec::new(),
            next_level_idx: 0,
        })
    }

    /// The number of levels declared in the options.
    pub fn num_levels(&self) -> usize {
        self.options.levels.len()
    }

    /// Write one level's batches (coarse → fine).
    ///
    /// `level_idx` MUST equal the number of levels already written (levels are
    /// written strictly in order 0, 1, 2, ...). Each input batch MUST have the
    /// source schema (no `level` column); this method appends the `level`
    /// column set to `level_idx`, GeoParquet-encodes the batch, and writes it.
    /// The level ends exactly on a row-group boundary (§4.2).
    ///
    /// `level_row_hint` is the total number of rows this level will contribute
    /// (H1). When supplied, the writer sizes this level's row groups from it:
    /// a level with `hint <= max_row_group_size` rows is written as a **single**
    /// row group; a larger level is split into `ceil(hint / max_row_group_size)`
    /// row groups of roughly uniform size. When `None`, the writer falls back to
    /// splitting every `max_row_group_size` rows. Either way the level ends
    /// exactly on a row-group boundary and never shares a row group with another
    /// level (§4.2) — the RG-boundary-per-level invariant is exact.
    ///
    /// A level whose batches yield **zero rows** is skipped, not an error
    /// (§7.3): nothing is written, the declared spec is dropped from the
    /// footer, and subsequent levels are renumbered down by one physical
    /// index (their `level` column values stay contiguous from 0). The
    /// returned [`LevelWriteOutcome`] tells the caller which case occurred.
    pub fn write_level(
        &mut self,
        level_idx: usize,
        level_row_hint: Option<usize>,
        batches: impl Iterator<Item = RecordBatch>,
    ) -> Result<LevelWriteOutcome, WriterError> {
        if level_idx != self.next_level_idx {
            return Err(WriterError::LevelOutOfOrder {
                expected: self.next_level_idx,
                got: level_idx,
            });
        }

        let rg_before = self.writer.flushed_row_groups().len();

        // Rows per row group for THIS level (§4.2, §4.5). The underlying
        // `ArrowWriter` never splits on its own (its `max_row_group_size` is set
        // to `usize::MAX` in `build_writer_properties`); we drive every row-group
        // boundary here by slicing each encoded batch to `target` and flushing.
        let cap = effective_rg_cap(
            self.options.max_row_group_size,
            self.options.row_group_size_policy,
            self.options.levels.get(level_idx).and_then(|l| l.zoom),
            self.options.levels.last().and_then(|l| l.zoom),
        );
        let target = rg_row_target(cap, level_row_hint);

        // Rows accumulated into the current (not-yet-flushed) row group.
        let mut in_rg: usize = 0;

        // The PHYSICAL level index this level will occupy in the output file:
        // the count of levels actually written so far. It trails `level_idx`
        // once an empty level has been skipped (§7.3 renumbering).
        let physical_idx = self.level_row_group_ends.len();

        for batch in batches {
            let num_rows = batch.num_rows();
            let level_array = Int32Array::from(vec![physical_idx as i32; num_rows]);

            // Drop the colliding covering column(s) (§4.4), then append `level`.
            let mut columns: Vec<_> = batch
                .columns()
                .iter()
                .enumerate()
                .filter(|(i, _)| !self.drop_indices.contains(i))
                .map(|(_, c)| c.clone())
                .collect();
            columns.push(Arc::new(level_array));
            let augmented = RecordBatch::try_new(self.augmented_schema.clone(), columns)?;

            let encoded = self
                .encoder
                .encode_record_batch(&augmented)
                .map_err(|e| WriterError::GeoParquet(e.to_string()))?;

            // Slice the encoded batch so row-group boundaries fall exactly on
            // `target`-row multiples (carrying `in_rg` across batches).
            let n = encoded.num_rows();
            let mut offset = 0usize;
            while offset < n {
                let take = (target - in_rg).min(n - offset);
                self.writer.write(&encoded.slice(offset, take))?;
                in_rg += take;
                offset += take;
                if in_rg >= target {
                    self.writer.flush()?;
                    in_rg = 0;
                }
            }
        }

        // Close the final partial row group so the level ends exactly on a
        // boundary (§4.2). If `in_rg == 0` the boundary already fell on the last
        // flush, so a trailing flush would create a spurious empty row group.
        if in_rg > 0 {
            self.writer.flush()?;
        }

        let rg_after = self.writer.flushed_row_groups().len();
        if rg_after <= rg_before {
            // Empty level: omit it entirely and renumber (§7.3). No rows were
            // written (a partial row group would have been flushed above), so
            // the file carries no trace of it; only the bookkeeping moves on.
            self.next_level_idx += 1;
            return Ok(LevelWriteOutcome::SkippedEmpty);
        }

        self.level_row_group_ends.push(rg_after as i64 - 1);
        self.written_spec_indices.push(level_idx);
        self.next_level_idx += 1;
        Ok(LevelWriteOutcome::Written)
    }

    /// Finalize the file: write the `geo` and `geo:overviews` footer keys (plus
    /// the optional `cogp` key), then close. Returns the footer metadata that
    /// was written.
    pub fn finish(mut self) -> Result<OverviewsMeta, WriterError> {
        if self.next_level_idx != self.options.levels.len() {
            return Err(WriterError::IncompleteLevels {
                written: self.next_level_idx,
                expected: self.options.levels.len(),
            });
        }

        // Every level may legally be skipped-as-empty individually, but a
        // file with NO levels is invalid (`levels` MUST be non-empty, §3.3):
        // fail with an actionable error instead of writing garbage.
        if self.level_row_group_ends.is_empty() {
            return Err(WriterError::AllLevelsEmpty {
                expected: self.options.levels.len(),
            });
        }

        let num_row_groups = self.writer.flushed_row_groups().len() as i64;
        let meta = self.build_meta();
        // Sanity: the writer's own output must satisfy the structural invariants.
        meta.validate(num_row_groups)?;

        // GeoParquet 1.1 `geo` metadata (covering + geometry types). Consumes
        // the encoder; done before other field borrows of `self`.
        let geo_kv = self
            .encoder
            .into_keyvalue()
            .map_err(|e| WriterError::GeoParquet(e.to_string()))?;
        self.writer.append_key_value_metadata(geo_kv);

        // `geo:overviews` footer key (§3).
        let overviews_json = meta.to_json()?;
        self.writer
            .append_key_value_metadata(KeyValue::new(OVERVIEWS_KEY.to_string(), overviews_json));

        // Optional COGP compatibility key (§3.1), behind the explicit flag.
        if self.options.cogp_compat_key {
            let cogp_json = meta.to_cogp_json()?;
            self.writer
                .append_key_value_metadata(KeyValue::new(COGP_KEY.to_string(), cogp_json));
        }

        self.writer.close()?;
        Ok(meta)
    }

    fn build_meta(&self) -> OverviewsMeta {
        // Only the levels actually written appear in the footer; skipped
        // (empty) levels drop their declared spec too, keeping `levels`,
        // `row_group_end`, and the physical `level` column aligned (§7.3).
        let levels: Vec<Level> = self
            .level_row_group_ends
            .iter()
            .zip(self.written_spec_indices.iter())
            .map(|(&row_group_end, &spec_idx)| Level {
                row_group_end,
                gsd: self.options.levels[spec_idx].gsd,
                zoom: self.options.levels[spec_idx].zoom,
            })
            .collect();

        let canonical_level = match self.options.mode {
            Mode::Duplicating => Some(levels.len() as i64 - 1),
            Mode::Partitioning => None,
        };

        // The provenance `levels` array is parallel to the top-level `levels`
        // (§3.5): drop the skipped entries there as well.
        let generalization = self.options.generalization.clone().map(|mut g| {
            if g.levels.len() == self.options.levels.len()
                && self.written_spec_indices.len() != self.options.levels.len()
            {
                g.levels = self
                    .written_spec_indices
                    .iter()
                    .map(|&i| g.levels[i].clone())
                    .collect();
            }
            g
        });

        OverviewsMeta {
            version: self.options.version.clone(),
            mode: Some(self.options.mode),
            canonical_level,
            levels,
            generalization,
        }
    }
}

/// Names of geometry columns in a schema (fields carrying a `geoarrow.*`
/// extension type). Mirrors the geoparquet encoder's detection.
fn geometry_columns(schema: &Schema) -> Vec<String> {
    schema
        .fields()
        .iter()
        .filter(|f| {
            f.metadata()
                .get(EXTENSION_TYPE_NAME_KEY)
                .is_some_and(|name| name.starts_with("geoarrow"))
        })
        .map(|f| f.name().clone())
        .collect()
}

/// Whether `field` is a bbox-covering struct: a `Struct` whose child field
/// names are exactly `{xmin, ymin, xmax, ymax}` (case-insensitive). Used to
/// recognise (and drop) a pre-existing covering column that would collide with
/// the encoder's generated one (§4.4), without touching unrelated struct
/// attributes that merely share the covering's *name*.
fn is_bbox_covering_struct(field: &Field) -> bool {
    match field.data_type() {
        DataType::Struct(children) => {
            if children.len() != 4 {
                return false;
            }
            let mut names: Vec<String> = children.iter().map(|c| c.name().to_lowercase()).collect();
            names.sort();
            names == ["xmax", "xmin", "ymax", "ymin"]
        }
        _ => false,
    }
}

/// Covering column name the encoder will produce for a geometry column, per its
/// default rule (`bbox` for `geometry`/`geography`, else `{name}_bbox`).
fn covering_name_for(column_name: &str) -> String {
    if column_name == "geometry" || column_name == "geography" {
        "bbox".to_string()
    } else {
        format!("{column_name}_bbox")
    }
}

/// Effective row-group cap for one level under a [`RowGroupSizePolicy`]
/// (#202). `Constant` (and any level/file without zoom metadata) returns the
/// base cap; `ZoomScaled` doubles it per zoom step below the finest level's
/// zoom, saturating at `usize::MAX`.
fn effective_rg_cap(
    base: usize,
    policy: RowGroupSizePolicy,
    level_zoom: Option<u8>,
    finest_zoom: Option<u8>,
) -> usize {
    match (policy, level_zoom, finest_zoom) {
        (RowGroupSizePolicy::ZoomScaled, Some(z), Some(zmax)) if z < zmax => {
            let steps = u32::from(zmax - z);
            if steps >= usize::BITS {
                usize::MAX
            } else {
                base.saturating_mul(1usize << steps)
            }
        }
        _ => base,
    }
}

/// Rows per row group for a level (H1). With a known `level_row_hint`:
/// a level that fits in `cap` rows becomes a **single** row group; a larger
/// level is split into `ceil(hint / cap)` row groups of roughly uniform size.
/// With `None`, fall back to the cap (split every `cap` rows).
fn rg_row_target(max_row_group_size: usize, level_row_hint: Option<usize>) -> usize {
    let cap = max_row_group_size.max(1);
    match level_row_hint {
        Some(n) if n > 0 => {
            if n <= cap {
                n
            } else {
                let num_rgs = n.div_ceil(cap);
                n.div_ceil(num_rgs)
            }
        }
        _ => cap,
    }
}

/// Build [`WriterProperties`]: ZSTD, no dictionary on geometry + bbox columns
/// (§4.5), manual per-level row-group control, and statistics tuned so the
/// footer stays small (H1) while the pruning index survives.
///
/// - `set_max_row_group_row_count(Some(usize::MAX))`: the writer never splits row groups on
///   its own; [`OverviewWriter::write_level`] drives every boundary so each
///   level is sized independently and ends on a row-group boundary (§4.2).
/// - Statistics: the bbox covering children and `level` column keep full stats
///   (the spatial-pruning index, §4.4). Unless `full_column_stats` is set, the
///   WKB geometry column and every Utf8/Binary property column have their stats
///   suppressed — their per-row-group min/max are never used by a reader but
///   dominate the footer on high-cardinality data (e.g. Overture ULID `id`).
fn build_writer_properties(
    options: &OverviewWriterOptions,
    geometry_columns: &[String],
    augmented_schema: &Schema,
) -> Result<WriterProperties, WriterError> {
    let mut builder = WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(options.zstd_level)?))
        // Manual per-level row-group control (see write_level): disable the
        // writer's own row-count splitting.
        .set_max_row_group_row_count(Some(usize::MAX))
        // Per-row-group (chunk) statistics are the spatial-pruning index (§4.4);
        // Page-level also carries chunk-level min/max.
        .set_statistics_enabled(EnabledStatistics::Page);

    let geom_set: std::collections::HashSet<&str> =
        geometry_columns.iter().map(|s| s.as_str()).collect();

    for geom in geometry_columns {
        // Geometry (WKB) column: no dictionary (§4.5).
        builder = builder.set_column_dictionary_enabled(ColumnPath::from(geom.clone()), false);

        // bbox covering struct children: no dictionary (§4.5).
        let covering = covering_name_for(geom);
        for child in ["xmin", "ymin", "xmax", "ymax"] {
            let path = ColumnPath::from(vec![covering.clone(), child.to_string()]);
            builder = builder.set_column_dictionary_enabled(path, false);
        }
    }

    // Statistics suppression on high-cardinality columns (H1). Suppress the WKB
    // geometry column and every string/binary property column; keep everything
    // else (covering children — a generated `bbox` struct not present in this
    // schema — and the `level` column both keep the default Page stats).
    if !options.full_column_stats {
        for field in augmented_schema.fields() {
            let name = field.name();
            let is_geometry = geom_set.contains(name.as_str());
            let is_string_or_binary = matches!(
                field.data_type(),
                DataType::Utf8
                    | DataType::LargeUtf8
                    | DataType::Utf8View
                    | DataType::Binary
                    | DataType::LargeBinary
                    | DataType::BinaryView
            );
            if is_geometry || is_string_or_binary {
                builder = builder.set_column_statistics_enabled(
                    ColumnPath::from(name.clone()),
                    EnabledStatistics::None,
                );
            }
        }
    }

    Ok(builder.build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::level::gsd;
    use arrow_array::{Array, BinaryArray, Int64Array, StringArray};
    use geo::{Geometry, LineString, Point, Polygon};
    use geoarrow::array::GeometryBuilder;
    use geoarrow::datatypes::GeometryType;
    use geoarrow_array::GeoArrowArray;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::file::statistics::Statistics;

    /// Deterministic geometry for a feature id: even ids are points, odd ids are
    /// square polygons. Keeps a mix of Point + Polygon in the column.
    fn geom_for(id: i64) -> Geometry {
        if id % 2 == 0 {
            Geometry::Point(Point::new(id as f64, id as f64))
        } else {
            let x = id as f64;
            let ext = LineString::from(vec![
                (x, x),
                (x + 1.0, x),
                (x + 1.0, x + 1.0),
                (x, x + 1.0),
                (x, x),
            ]);
            Geometry::Polygon(Polygon::new(ext, vec![]))
        }
    }

    /// Build the fixed source schema (id: Int64, name: Utf8, geometry: GeoArrow).
    fn source_schema() -> Schema {
        let geom_field = geometry_field();
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            geom_field,
        ])
    }

    /// A GeoArrow "geometry" field (mixed Geometry type, XY).
    fn geometry_field() -> Field {
        let arr = build_geometry_array(&[0]);
        arr.data_type().to_field("geometry", true)
    }

    fn build_geometry_array(ids: &[i64]) -> geoarrow::array::GeometryArray {
        let geoms: Vec<Option<Geometry>> = ids.iter().map(|&id| Some(geom_for(id))).collect();
        let typ = GeometryType::new(Default::default());
        let mut builder = GeometryBuilder::new(typ).with_prefer_multi(false);
        builder.extend_from_iter(geoms.iter().map(|x| x.as_ref()));
        builder.finish()
    }

    /// Build a source-schema record batch for the given feature ids.
    fn source_batch(schema: &SchemaRef, ids: &[i64]) -> RecordBatch {
        let id_array = Int64Array::from(ids.to_vec());
        let name_array =
            StringArray::from(ids.iter().map(|id| format!("f{id}")).collect::<Vec<_>>());
        let geom_array = build_geometry_array(ids);
        RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(id_array),
                Arc::new(name_array),
                Arc::new(geom_array.to_array_ref()),
            ],
        )
        .unwrap()
    }

    fn duplicating_options() -> OverviewWriterOptions {
        OverviewWriterOptions::new(
            Mode::Duplicating,
            vec![
                LevelSpec::new(gsd(2), Some(2)),
                LevelSpec::new(gsd(4), Some(4)),
                LevelSpec::new(gsd(6), Some(6)),
            ],
        )
    }

    /// Read the `level` column values for a single row group.
    fn read_level_column(path: &std::path::Path, rg: usize) -> Vec<i32> {
        let file = File::open(path).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let reader = builder.with_row_groups(vec![rg]).build().unwrap();
        let mut out = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            let idx = batch.schema().index_of(LEVEL_COLUMN).unwrap();
            let col = batch
                .column(idx)
                .as_any()
                .downcast_ref::<Int32Array>()
                .unwrap();
            out.extend(col.values().iter().copied());
        }
        out
    }

    #[test]
    fn schema_with_level_column_is_rejected() {
        let schema = Schema::new(vec![
            Field::new("level", DataType::Int32, false),
            geometry_field(),
        ]);
        let opts = OverviewWriterOptions::new(Mode::Duplicating, vec![LevelSpec::new(100.0, None)]);
        let sink: Vec<u8> = Vec::new();
        let result = OverviewWriter::try_new(sink, &schema, opts);
        assert!(matches!(result, Err(WriterError::LevelColumnExists)));
    }

    #[test]
    fn schema_with_case_colliding_level_column_is_rejected() {
        // V1 finding F2: Natural Earth admin data carries a `LEVEL`
        // attribute; DuckDB resolves identifiers case-insensitively,
        // so this must be rejected like an exact match.
        let schema = Schema::new(vec![
            Field::new("LEVEL", DataType::Int32, false),
            geometry_field(),
        ]);
        let opts = OverviewWriterOptions::new(Mode::Duplicating, vec![LevelSpec::new(100.0, None)]);
        let sink: Vec<u8> = Vec::new();
        let result = OverviewWriter::try_new(sink, &schema, opts);
        assert!(matches!(result, Err(WriterError::LevelColumnExists)));
    }

    #[test]
    fn write_level_out_of_order_is_rejected() {
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            OverviewWriter::create(tmp.path(), &schema, duplicating_options()).unwrap();
        // Skip level 0 -> should error.
        let batch = source_batch(&schema, &[0, 1]);
        let err = writer
            .write_level(1, None, std::iter::once(batch))
            .unwrap_err();
        assert!(matches!(
            err,
            WriterError::LevelOutOfOrder {
                expected: 0,
                got: 1
            }
        ));
    }

    #[test]
    fn empty_level_is_skipped_and_renumbered() {
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            OverviewWriter::create(tmp.path(), &schema, duplicating_options()).unwrap();

        // Level 0 (z2) yields no rows: skipped, not an error (§7.3, #211).
        let outcome = writer
            .write_level(0, Some(0), std::iter::empty::<RecordBatch>())
            .unwrap();
        assert_eq!(outcome, LevelWriteOutcome::SkippedEmpty);
        assert_eq!(
            writer
                .write_level(1, None, std::iter::once(source_batch(&schema, &[0, 1])))
                .unwrap(),
            LevelWriteOutcome::Written
        );
        assert_eq!(
            writer
                .write_level(
                    2,
                    None,
                    std::iter::once(source_batch(&schema, &[0, 1, 2, 3]))
                )
                .unwrap(),
            LevelWriteOutcome::Written
        );
        let meta = writer.finish().unwrap();

        // Footer: two levels carrying the z4/z6 specs; canonical renumbered.
        assert_eq!(meta.levels.len(), 2);
        assert_eq!(meta.levels[0].zoom, Some(4));
        assert_eq!(meta.levels[1].zoom, Some(6));
        assert_eq!(meta.levels[0].gsd, gsd(4));
        assert_eq!(meta.levels[1].gsd, gsd(6));
        assert_eq!(meta.canonical_level, Some(1));

        // The physical `level` column is renumbered contiguously from 0.
        assert_eq!(read_level_column(tmp.path(), 0), vec![0, 0]);
        assert_eq!(read_level_column(tmp.path(), 1), vec![1, 1, 1, 1]);
    }

    #[test]
    fn finish_with_all_levels_empty_errors() {
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            OverviewWriter::create(tmp.path(), &schema, duplicating_options()).unwrap();
        for level in 0..3 {
            let outcome = writer
                .write_level(level, Some(0), std::iter::empty::<RecordBatch>())
                .unwrap();
            assert_eq!(outcome, LevelWriteOutcome::SkippedEmpty);
        }
        let err = writer.finish().unwrap_err();
        assert!(
            matches!(err, WriterError::AllLevelsEmpty { expected: 3 }),
            "got {err:?}"
        );
    }

    #[test]
    fn preexisting_bbox_covering_is_not_duplicated() {
        use arrow_array::{Float64Array, StructArray};
        use arrow_schema::Fields;

        // is_bbox_covering_struct recognises the covering shape.
        let bbox_children = Fields::from(vec![
            Field::new("xmin", DataType::Float64, false),
            Field::new("ymin", DataType::Float64, false),
            Field::new("xmax", DataType::Float64, false),
            Field::new("ymax", DataType::Float64, false),
        ]);
        assert!(is_bbox_covering_struct(&Field::new(
            "bbox",
            DataType::Struct(bbox_children.clone()),
            false
        )));
        assert!(!is_bbox_covering_struct(&Field::new(
            "name",
            DataType::Utf8,
            false
        )));

        // Source schema mirrors a gpio-optimized input: it already carries a
        // `bbox` covering struct that collides with the encoder's generated one.
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            geometry_field(),
            Field::new("bbox", DataType::Struct(bbox_children.clone()), false),
        ]));

        let make_batch = |ids: &[i64]| {
            let id_array = Int64Array::from(ids.to_vec());
            let geom_array = build_geometry_array(ids);
            // Deliberately STALE covering sentinels (-999/999): if the writer
            // passed this column through instead of dropping it, these values
            // would surface in the covering stats.
            let col = |v: f64| Arc::new(Float64Array::from(vec![v; ids.len()])) as _;
            let bbox = StructArray::new(
                bbox_children.clone(),
                vec![col(-999.0), col(-999.0), col(999.0), col(999.0)],
                None,
            );
            RecordBatch::try_new(
                schema.clone(),
                vec![
                    Arc::new(id_array),
                    Arc::new(geom_array.to_array_ref()),
                    Arc::new(bbox),
                ],
            )
            .unwrap()
        };

        let tmp = tempfile::NamedTempFile::new().unwrap();
        {
            let mut writer =
                OverviewWriter::create(tmp.path(), &schema, duplicating_options()).unwrap();
            let _ = writer
                .write_level(0, None, std::iter::once(make_batch(&[0, 3])))
                .unwrap();
            let _ = writer
                .write_level(1, None, std::iter::once(make_batch(&[0, 1, 3])))
                .unwrap();
            let _ = writer
                .write_level(2, None, std::iter::once(make_batch(&[0, 1, 2, 3])))
                .unwrap();
            writer.finish().unwrap();
        }

        let file = File::open(tmp.path()).unwrap();
        let parquet_meta = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .metadata()
            .clone();

        // Exactly ONE physical `bbox.xmin` column — no `bbox`/`bbox_1`
        // duplication from passing the input covering through.
        let n_bbox_xmin = parquet_meta
            .row_group(0)
            .columns()
            .iter()
            .filter(|c| c.column_path().string() == "bbox.xmin")
            .count();
        assert_eq!(n_bbox_xmin, 1, "duplicate bbox covering column present");

        // The covering carries the encoder's FRESH geometry bounds, not the
        // stale -999/999 sentinels from the input column.
        for row_group in parquet_meta.row_groups() {
            for child in ["xmin", "ymin", "xmax", "ymax"] {
                let path = format!("bbox.{child}");
                let chunk = row_group
                    .columns()
                    .iter()
                    .find(|c| c.column_path().string() == path)
                    .unwrap_or_else(|| panic!("missing covering column {path}"));
                match chunk.statistics().expect("covering stats") {
                    Statistics::Double(s) => {
                        assert!(
                            *s.min_opt().unwrap() > -100.0 && *s.max_opt().unwrap() < 100.0,
                            "covering {path} carries stale input bbox values"
                        );
                    }
                    other => panic!("unexpected covering stats type: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn end_to_end_duplicating_roundtrip() {
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();

        // Level 2 (canonical) is the full source set; coarser levels are subsets.
        let level0_ids = vec![0i64, 3];
        let level1_ids = vec![0i64, 1, 3, 4];
        let canonical_ids = vec![0i64, 1, 2, 3, 4, 5];

        let written_meta = {
            let mut writer =
                OverviewWriter::create(tmp.path(), &schema, duplicating_options()).unwrap();
            let _ = writer
                .write_level(0, None, std::iter::once(source_batch(&schema, &level0_ids)))
                .unwrap();
            let _ = writer
                .write_level(1, None, std::iter::once(source_batch(&schema, &level1_ids)))
                .unwrap();
            let _ = writer
                .write_level(
                    2,
                    None,
                    std::iter::once(source_batch(&schema, &canonical_ids)),
                )
                .unwrap();
            writer.finish().unwrap()
        };

        // Footer metadata shape.
        assert_eq!(written_meta.mode, Some(Mode::Duplicating));
        assert_eq!(written_meta.canonical_level, Some(2));
        assert_eq!(
            written_meta
                .levels
                .iter()
                .map(|l| l.row_group_end)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );

        // Re-open and inspect the physical file.
        let file = File::open(tmp.path()).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let parquet_meta = builder.metadata().clone();
        assert_eq!(parquet_meta.num_row_groups(), 3);

        // RG boundaries align with declared row_group_end; level column min/max
        // per RG equals the level index; bbox covering has per-RG min/max stats.
        for (rg_idx, row_group) in parquet_meta.row_groups().iter().enumerate() {
            // level column values in this RG all equal rg_idx.
            let levels = read_level_column(tmp.path(), rg_idx);
            assert!(!levels.is_empty());
            assert!(levels.iter().all(|&v| v == rg_idx as i32));

            // level column statistics min == max == rg_idx.
            let level_col = row_group
                .columns()
                .iter()
                .find(|c| c.column_path().string() == LEVEL_COLUMN)
                .expect("level column chunk");
            match level_col.statistics().expect("level stats") {
                Statistics::Int32(s) => {
                    assert_eq!(*s.min_opt().unwrap(), rg_idx as i32);
                    assert_eq!(*s.max_opt().unwrap(), rg_idx as i32);
                }
                other => panic!("unexpected level stats type: {other:?}"),
            }

            // bbox covering child columns carry per-RG min/max statistics.
            for child in ["xmin", "ymin", "xmax", "ymax"] {
                let path = format!("bbox.{child}");
                let chunk = row_group
                    .columns()
                    .iter()
                    .find(|c| c.column_path().string() == path)
                    .unwrap_or_else(|| panic!("missing covering column {path}"));
                let stats = chunk
                    .statistics()
                    .unwrap_or_else(|| panic!("no stats for {path}"));
                assert!(
                    stats.min_bytes_opt().is_some() && stats.max_bytes_opt().is_some(),
                    "covering {path} missing min/max stats"
                );
            }
        }

        // `geo` metadata parses with covering declared.
        let geo = geoparquet::metadata::GeoParquetMetadata::from_parquet_meta(
            parquet_meta.file_metadata(),
        )
        .expect("geo metadata present")
        .expect("geo metadata parses");
        let geom_col = geo.columns.get("geometry").expect("geometry column meta");
        assert!(geom_col.covering.is_some(), "covering not declared");
        // Union of geometry types is recorded (Point + Polygon).
        let type_names: Vec<String> = geom_col
            .geometry_types
            .iter()
            .map(|t| format!("{t:?}"))
            .collect();
        assert!(
            geom_col.geometry_types.len() >= 2,
            "expected Point+Polygon union, got {type_names:?}"
        );

        // `geo:overviews` JSON present and equal to what we wrote.
        let ov_kv = parquet_meta
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .find(|kv| kv.key == OVERVIEWS_KEY)
            .expect("geo:overviews key present");
        let parsed = OverviewsMeta::from_json(ov_kv.value.as_ref().unwrap()).unwrap();
        assert_eq!(parsed, written_meta);

        // No cogp key by default.
        assert!(parquet_meta
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .all(|kv| kv.key != COGP_KEY));

        // Canonical-level rows (RG 2) re-read value-identical to the input.
        let file = File::open(tmp.path()).unwrap();
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .with_row_groups(vec![2])
            .build()
            .unwrap();
        let mut got_ids = Vec::new();
        let mut got_names = Vec::new();
        let mut got_geoms = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            let ids = batch
                .column(batch.schema().index_of("id").unwrap())
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            got_ids.extend(ids.values().iter().copied());
            let names = batch
                .column(batch.schema().index_of("name").unwrap())
                .as_any()
                .downcast_ref::<StringArray>()
                .unwrap();
            for i in 0..names.len() {
                got_names.push(names.value(i).to_string());
            }
            let geom = batch
                .column(batch.schema().index_of("geometry").unwrap())
                .as_any()
                .downcast_ref::<BinaryArray>()
                .expect("geometry stored as WKB Binary");
            for i in 0..geom.len() {
                got_geoms.push(crate::wkb::wkb_to_geometry(geom.value(i)).unwrap());
            }
        }
        assert_eq!(got_ids, canonical_ids);
        assert_eq!(
            got_names,
            canonical_ids
                .iter()
                .map(|id| format!("f{id}"))
                .collect::<Vec<_>>()
        );
        let expected_geoms: Vec<Geometry> = canonical_ids.iter().map(|&id| geom_for(id)).collect();
        assert_eq!(got_geoms, expected_geoms, "canonical geometry not verbatim");
    }

    #[test]
    fn cogp_flag_emits_compat_key() {
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();

        let mut opts = OverviewWriterOptions::new(
            Mode::Partitioning,
            vec![
                LevelSpec::new(1000.0, Some(6)),
                LevelSpec::new(500.0, Some(7)),
            ],
        );
        opts.cogp_compat_key = true;

        let written_meta = {
            let mut writer = OverviewWriter::create(tmp.path(), &schema, opts).unwrap();
            let _ = writer
                .write_level(0, None, std::iter::once(source_batch(&schema, &[0, 2])))
                .unwrap();
            let _ = writer
                .write_level(1, None, std::iter::once(source_batch(&schema, &[1, 3, 5])))
                .unwrap();
            writer.finish().unwrap()
        };

        // Partitioning: canonical_level is null.
        assert_eq!(written_meta.mode, Some(Mode::Partitioning));
        assert_eq!(written_meta.canonical_level, None);

        let file = File::open(tmp.path()).unwrap();
        let builder = ParquetRecordBatchReaderBuilder::try_new(file).unwrap();
        let kvs = builder
            .metadata()
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .clone();

        let cogp = kvs
            .iter()
            .find(|kv| kv.key == COGP_KEY)
            .expect("cogp key present when flag on");
        let v: serde_json::Value = serde_json::from_str(cogp.value.as_ref().unwrap()).unwrap();
        assert_eq!(v["version"], SPEC_VERSION);
        assert_eq!(v["levels"][0]["row_group_end"], 0);
        assert_eq!(v["levels"][1]["row_group_end"], 1);
        assert!(v["levels"][0].get("zoom").is_none());
        // Agrees with the authoritative geo:overviews key.
        let ov = kvs.iter().find(|kv| kv.key == OVERVIEWS_KEY).unwrap();
        let parsed = OverviewsMeta::from_json(ov.value.as_ref().unwrap()).unwrap();
        assert_eq!(parsed, written_meta);
        for (i, lvl) in parsed.levels.iter().enumerate() {
            assert_eq!(v["levels"][i]["row_group_end"], lvl.row_group_end);
            assert_eq!(v["levels"][i]["gsd"], lvl.gsd);
        }
    }

    #[test]
    fn finish_before_all_levels_is_rejected() {
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut writer =
            OverviewWriter::create(tmp.path(), &schema, duplicating_options()).unwrap();
        let _ = writer
            .write_level(0, None, std::iter::once(source_batch(&schema, &[0, 3])))
            .unwrap();
        let err = writer.finish().unwrap_err();
        assert!(matches!(
            err,
            WriterError::IncompleteLevels {
                written: 1,
                expected: 3
            }
        ));
    }

    #[test]
    fn rg_row_target_sizes_levels() {
        // Small level (<= cap): one row group of exactly the level's rows.
        assert_eq!(rg_row_target(10_000, Some(3)), 3);
        assert_eq!(rg_row_target(10_000, Some(10_000)), 10_000);
        // Large level: ceil(n/cap) uniform row groups.
        // 10 rows, cap 4 -> ceil(10/4)=3 groups -> ceil(10/3)=4 rows/group.
        assert_eq!(rg_row_target(4, Some(10)), 4);
        // 9 rows, cap 4 -> 3 groups -> 3 rows/group (more uniform than 4,4,1).
        assert_eq!(rg_row_target(4, Some(9)), 3);
        // Unknown hint falls back to the cap.
        assert_eq!(rg_row_target(4, None), 4);
        assert_eq!(rg_row_target(4, Some(0)), 4);
    }

    #[test]
    fn per_level_row_group_sizing() {
        // A small coarse band becomes ONE row group; a larger fine band splits
        // into several roughly uniform row groups. Footer row_group_end values
        // stay exact and the file validates.
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut opts = OverviewWriterOptions::new(
            Mode::Duplicating,
            vec![
                LevelSpec::new(gsd(2), Some(2)),
                LevelSpec::new(gsd(4), Some(4)),
            ],
        );
        opts.max_row_group_size = 4;

        let meta = {
            let mut writer = OverviewWriter::create(tmp.path(), &schema, opts).unwrap();
            // Level 0: 3 rows (<= cap 4) -> single row group.
            let _ = writer
                .write_level(
                    0,
                    Some(3),
                    std::iter::once(source_batch(&schema, &[0, 1, 2])),
                )
                .unwrap();
            // Level 1: 10 rows (> cap 4) -> ceil(10/4)=3 uniform row groups.
            let ids: Vec<i64> = (0..10).collect();
            let _ = writer
                .write_level(
                    1,
                    Some(ids.len()),
                    std::iter::once(source_batch(&schema, &ids)),
                )
                .unwrap();
            writer.finish().unwrap()
        };

        // Footer bands: level 0 = RG 0; level 1 = RGs 1..=3.
        assert_eq!(meta.levels[0].row_group_end, 0);
        assert_eq!(meta.levels[1].row_group_end, 3);

        let file = File::open(tmp.path()).unwrap();
        let pm = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .metadata()
            .clone();
        assert_eq!(pm.num_row_groups(), 4);
        // Level 0 is a single row group of all 3 rows.
        assert_eq!(pm.row_group(0).num_rows(), 3);
        // Level 1's three row groups sum to 10 and none exceeds the cap.
        let l1: Vec<i64> = (1..=3).map(|r| pm.row_group(r).num_rows()).collect();
        assert_eq!(l1.iter().sum::<i64>(), 10);
        assert!(l1.iter().all(|&r| r <= 4), "row groups exceed cap: {l1:?}");

        // Every row group is single-level (invariant §4.2) and validates.
        assert!(crate::overview::check::validate_file(tmp.path())
            .unwrap()
            .is_valid());
    }

    #[test]
    fn effective_rg_cap_scales_with_zoom_distance() {
        use RowGroupSizePolicy::{Constant, ZoomScaled};
        // Constant: the base cap regardless of zooms.
        assert_eq!(effective_rg_cap(4, Constant, Some(2), Some(4)), 4);
        // ZoomScaled: doubles per zoom step below the finest level's zoom.
        assert_eq!(effective_rg_cap(4, ZoomScaled, Some(4), Some(4)), 4);
        assert_eq!(effective_rg_cap(4, ZoomScaled, Some(3), Some(4)), 8);
        assert_eq!(effective_rg_cap(4, ZoomScaled, Some(2), Some(4)), 16);
        // Missing zoom metadata falls back to the base cap.
        assert_eq!(effective_rg_cap(4, ZoomScaled, None, Some(4)), 4);
        assert_eq!(effective_rg_cap(4, ZoomScaled, Some(2), None), 4);
        // Saturates instead of overflowing on absurd zoom spans.
        assert_eq!(
            effective_rg_cap(usize::MAX / 2, ZoomScaled, Some(0), Some(30)),
            usize::MAX
        );
    }

    #[test]
    fn zoom_scaled_policy_widens_coarse_level_caps() {
        // Two levels of 10 rows each, base cap 4. Under the constant policy
        // BOTH levels split into 3 row groups. Under zoom-scaled the coarse
        // level (z2, two steps below the finest z4) gets cap 4<<2 = 16 and
        // becomes a SINGLE row group, while the finest level keeps cap 4.
        let schema = Arc::new(source_schema());
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut opts = OverviewWriterOptions::new(
            Mode::Duplicating,
            vec![
                LevelSpec::new(gsd(2), Some(2)),
                LevelSpec::new(gsd(4), Some(4)),
            ],
        );
        opts.max_row_group_size = 4;
        opts.row_group_size_policy = RowGroupSizePolicy::ZoomScaled;

        let ids: Vec<i64> = (0..10).collect();
        let meta = {
            let mut writer = OverviewWriter::create(tmp.path(), &schema, opts).unwrap();
            let _ = writer
                .write_level(
                    0,
                    Some(ids.len()),
                    std::iter::once(source_batch(&schema, &ids)),
                )
                .unwrap();
            let _ = writer
                .write_level(
                    1,
                    Some(ids.len()),
                    std::iter::once(source_batch(&schema, &ids)),
                )
                .unwrap();
            writer.finish().unwrap()
        };

        // Coarse band: one row group (RG 0); fine band: RGs 1..=3.
        assert_eq!(meta.levels[0].row_group_end, 0);
        assert_eq!(meta.levels[1].row_group_end, 3);

        let file = File::open(tmp.path()).unwrap();
        let pm = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .metadata()
            .clone();
        assert_eq!(pm.num_row_groups(), 4);
        assert_eq!(pm.row_group(0).num_rows(), 10);
        let l1: Vec<i64> = (1..=3).map(|r| pm.row_group(r).num_rows()).collect();
        assert_eq!(l1.iter().sum::<i64>(), 10);
        assert!(
            l1.iter().all(|&r| r <= 4),
            "fine row groups exceed cap: {l1:?}"
        );

        // Level↔row-group alignment invariants still hold.
        assert!(crate::overview::check::validate_file(tmp.path())
            .unwrap()
            .is_valid());
    }

    /// Does the row group at index `rg` have statistics for the column at
    /// `path`? Helper for the stats-suppression tests.
    fn rg_has_stats(path: &std::path::Path, rg: usize, col_path: &str) -> bool {
        let file = File::open(path).unwrap();
        let pm = ParquetRecordBatchReaderBuilder::try_new(file)
            .unwrap()
            .metadata()
            .clone();
        pm.row_group(rg)
            .columns()
            .iter()
            .find(|c| c.column_path().string() == col_path)
            .unwrap_or_else(|| panic!("column {col_path} not found"))
            .statistics()
            .is_some()
    }

    fn write_three_level_file(path: &std::path::Path, full_column_stats: bool) {
        let schema = Arc::new(source_schema());
        let mut opts = duplicating_options();
        opts.full_column_stats = full_column_stats;
        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        let _ = writer
            .write_level(0, Some(2), std::iter::once(source_batch(&schema, &[0, 3])))
            .unwrap();
        let _ = writer
            .write_level(
                1,
                Some(3),
                std::iter::once(source_batch(&schema, &[0, 1, 3])),
            )
            .unwrap();
        let _ = writer
            .write_level(
                2,
                Some(4),
                std::iter::once(source_batch(&schema, &[0, 1, 2, 3])),
            )
            .unwrap();
        writer.finish().unwrap();
    }

    #[test]
    fn string_and_geometry_stats_suppressed_by_default() {
        // Default: the bbox covering + level column keep their pruning stats;
        // the Utf8 `name` and WKB `geometry` columns have their stats suppressed
        // (the H1 footer fix). The numeric `id` column keeps its (cheap) stats.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_three_level_file(tmp.path(), false);

        // Pruning index: MUST be present.
        assert!(rg_has_stats(tmp.path(), 0, "level"), "level stats missing");
        for child in ["xmin", "ymin", "xmax", "ymax"] {
            assert!(
                rg_has_stats(tmp.path(), 0, &format!("bbox.{child}")),
                "covering bbox.{child} stats missing"
            );
        }
        // High-cardinality string + WKB geometry: MUST be suppressed.
        assert!(
            !rg_has_stats(tmp.path(), 0, "name"),
            "string property stats not suppressed"
        );
        assert!(
            !rg_has_stats(tmp.path(), 0, "geometry"),
            "geometry WKB stats not suppressed"
        );
        // Numeric property keeps stats (cheap, occasionally useful).
        assert!(
            rg_has_stats(tmp.path(), 0, "id"),
            "numeric id stats missing"
        );
    }

    #[test]
    fn full_column_stats_flag_keeps_all_stats() {
        // With --full-column-stats every column keeps stats, including the
        // string `name` and WKB `geometry` columns.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_three_level_file(tmp.path(), true);

        assert!(rg_has_stats(tmp.path(), 0, "level"));
        assert!(rg_has_stats(tmp.path(), 0, "bbox.xmin"));
        assert!(
            rg_has_stats(tmp.path(), 0, "name"),
            "string stats should be kept under full_column_stats"
        );
        assert!(
            rg_has_stats(tmp.path(), 0, "geometry"),
            "geometry stats should be kept under full_column_stats"
        );
    }
}
