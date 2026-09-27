//! Reader for GeoParquet overview files (spec §5).
//!
//! [`OverviewReader`] parses a file's Parquet footer once, extracts and parses
//! the [`OVERVIEWS_KEY`] (`geo:overviews`) footer key into an [`OverviewsMeta`],
//! and provides the spec §5 read protocol: level selection by target GSD/zoom,
//! per-row-group bbox pruning against the covering column's statistics, and
//! reading exactly the surviving row groups of a level.
//!
//! ## Byte source
//!
//! v0.1 targets **local files**. The design keeps the byte source swappable: the
//! footer is parsed once in [`OverviewReader::open`], and each
//! [`OverviewReader::read_level`] re-opens the backing path to build a
//! row-group-scoped reader (mirroring `batch_processor`'s read path). A future
//! `object_store`/HTTP variant swaps only the open + row-group fetch — the level
//! selection and pruning logic ([`OverviewReader::level_for_gsd`],
//! [`OverviewReader::selected_row_groups`]) operate purely on parsed metadata and
//! carry over unchanged.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_schema::SchemaRef;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, ParquetRecordBatchReaderBuilder};
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::ParquetMetaData;

use crate::covering::extract_row_group_bounds_from_metadata;
use crate::tile::TileBounds;

use super::level::{gsd, Mode, OverviewsMeta, OVERVIEWS_KEY};

/// Errors produced when opening or reading an overview file.
#[derive(Debug, thiserror::Error)]
pub enum ReaderError {
    /// I/O error opening the backing file.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Underlying parquet error (footer parse, row-group read).
    #[error("parquet error: {0}")]
    Parquet(#[from] parquet::errors::ParquetError),
    /// The `geo:overviews` footer key is absent — not an overview file.
    #[error("'geo:overviews' footer key absent: not an overview file")]
    MissingOverviewsKey,
    /// The `geo:overviews` footer key is present but is not valid JSON per §3.
    #[error("invalid 'geo:overviews' JSON: {0}")]
    InvalidOverviewsJson(serde_json::Error),
    /// The `geo:overviews` metadata parses but violates a §3.3/§3.4
    /// structural invariant against the file's actual row groups (level band
    /// out of range / non-monotonic / footer-data mismatch): the level bands
    /// cannot be trusted, so the file is rejected at open (H4 hardening — a
    /// hostile `row_group_end` would otherwise drive band arithmetic).
    #[error("invalid 'geo:overviews' metadata: {0}")]
    InvalidMetadata(#[from] super::level::OverviewValidationError),
    /// A level index was requested that does not exist in the file.
    #[error("level {level} out of range (file has {num_levels} levels)")]
    LevelOutOfRange {
        /// The offending level index.
        level: usize,
        /// The number of levels in the file.
        num_levels: usize,
    },
}

/// A reader over a local GeoParquet overview file.
///
/// Constructed with [`OverviewReader::open`], which parses the footer once.
#[derive(Debug, Clone)]
pub struct OverviewReader {
    path: PathBuf,
    metadata: Arc<ParquetMetaData>,
    schema: SchemaRef,
    meta: OverviewsMeta,
    /// Most rows one read batch may hold (#563): an overview level keeps its
    /// source geometries, so a batch of very large ones would pass arrow's
    /// i32 byte-array offset ceiling. Every read clamps its batch to this.
    max_batch_rows: usize,
}

impl OverviewReader {
    /// Open a local overview file and parse its footer.
    ///
    /// Parses the Parquet footer once and extracts the [`OVERVIEWS_KEY`] footer
    /// key into an [`OverviewsMeta`]. Returns [`ReaderError::MissingOverviewsKey`]
    /// if the key is absent and [`ReaderError::InvalidOverviewsJson`] if it does
    /// not parse.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, ReaderError> {
        let path = path.as_ref().to_path_buf();
        let file = File::open(&path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?;
        let metadata = builder.metadata().clone();
        let schema = builder.schema().clone();

        let json = metadata
            .file_metadata()
            .key_value_metadata()
            .and_then(|kvs| kvs.iter().find(|kv| kv.key == OVERVIEWS_KEY))
            .and_then(|kv| kv.value.clone())
            .ok_or(ReaderError::MissingOverviewsKey)?;

        let meta = OverviewsMeta::from_json(&json).map_err(ReaderError::InvalidOverviewsJson)?;

        // Structural validation against the file's ACTUAL row groups (§3.3 /
        // §3.4): every subsequent band computation trusts the footer's
        // row_group_end values, so a corrupt or tampered footer must be
        // rejected here rather than driving out-of-range (or, for negative
        // values, usize-wrapped) row-group reads.
        meta.validate(metadata.num_row_groups() as i64)?;

        let max_batch_rows = crate::input_set::capped_batch_rows(
            usize::MAX,
            crate::input_set::widest_byte_array_column(&metadata, None, 0).as_ref(),
        );
        Ok(Self {
            max_batch_rows,
            path,
            metadata,
            schema,
            meta,
        })
    }

    /// The parsed `geo:overviews` footer metadata.
    pub fn meta(&self) -> &OverviewsMeta {
        &self.meta
    }

    /// The raw GeoParquet `geo` footer JSON, if the file carries one (an
    /// overview written by tylertoo always does; a foreign file may not).
    pub fn geo_metadata_json(&self) -> Option<&str> {
        self.metadata
            .file_metadata()
            .key_value_metadata()?
            .iter()
            .find(|kv| kv.key.eq_ignore_ascii_case("geo"))?
            .value
            .as_deref()
    }

    /// The total number of Parquet row groups in the file.
    pub fn num_row_groups(&self) -> usize {
        self.metadata.num_row_groups()
    }

    /// Mean **uncompressed** bytes per row in the finest (highest-index) level's
    /// row-group band, read straight from the Parquet metadata (no scan, no
    /// decode).
    ///
    /// This is the data-aware per-member size signal the export memory preflight
    /// (#311) multiplies by the densest partition's member count to size a
    /// per-partition transient — replacing the flat 64 MiB assumption that
    /// OOM-killed dense finest-zoom exports. The finest level is chosen because
    /// it carries full-resolution geometry and is where the export OOMs; its
    /// stored bytes/row (geoarrow native `f64` coordinates + properties, no
    /// column compression) is a high-biased proxy for a resident clipped
    /// member's cost, which suits a preflight that must bias narrow.
    ///
    /// Returns `None` when the finest level has no rows (empty file) so callers
    /// fall back to the calibrated constant rather than dividing by zero.
    pub fn finest_level_mean_row_bytes(&self) -> Option<u64> {
        let finest = self.num_levels().checked_sub(1)?;
        let (start, end) = self.level_band(finest).ok()?;
        let mut bytes: u64 = 0;
        let mut rows: i64 = 0;
        for rg in start..=end {
            let rgm = self.metadata.row_group(rg);
            bytes = bytes.saturating_add(rgm.total_byte_size().max(0) as u64);
            rows = rows.saturating_add(rgm.num_rows());
        }
        if rows <= 0 {
            return None;
        }
        Some(bytes / rows as u64)
    }

    /// The largest value of integer column `name` across the whole file, read
    /// from row-group statistics alone (no data pages).
    ///
    /// `Some(v)` means the column holds `v` on every row: the fold requires
    /// the file-wide min and max statistics to agree. `None` when the column
    /// is absent, is not a *signed* INT32/INT64, spans more than one value,
    /// or any non-empty row group lacks a min/max statistic or holds a null
    /// — the caller then knows nothing and must treat the column as carrying
    /// information. Used to recognise provenance counters that never left 1
    /// (#379).
    ///
    /// The refusals exist for files the converter did not write (#399); its
    /// own counters are signed INT32 NOT NULL with chunk statistics, so they
    /// always fold:
    ///
    /// - An unsigned logical type (`UInt32` over INT32, `UInt64` over INT64)
    ///   stores values ≥ 2^31 / 2^63 as negative physical ints, so a signed
    ///   fold would put a row group whose real max is huge *below* one whose
    ///   max is 1 and the caller would withhold a column that is plainly not
    ///   1 everywhere. Reinterpreting the bits is not enough either: whether
    ///   a writer computed the chunk min/max with unsigned ordering is a
    ///   per-writer choice (`ColumnOrder`), and a heuristic whose only cost
    ///   on `None` is "export the column" has no business trusting it.
    /// - A row group with `null_count > 0` (or, for an OPTIONAL column, no
    ///   recorded null count) may hold `{1, null}`: its max is 1, but the
    ///   null/1 distinction is information the tiles would lose.
    pub fn int_column_max(&self, name: &str) -> Option<i64> {
        use parquet::basic::{ConvertedType, IntType, LogicalType};
        use parquet::file::statistics::Statistics;

        let descr = self.metadata.file_metadata().schema_descr();
        let col_idx =
            (0..descr.num_columns()).find(|&i| descr.column(i).path().string() == name)?;
        let column = descr.column(col_idx);
        let unsigned = matches!(
            column.logical_type_ref(),
            Some(LogicalType::Integer(IntType {
                is_signed: false,
                ..
            }))
        ) || matches!(
            column.converted_type(),
            ConvertedType::UINT_8
                | ConvertedType::UINT_16
                | ConvertedType::UINT_32
                | ConvertedType::UINT_64
        );
        if unsigned {
            return None;
        }
        // A definition level above zero means the leaf, or an ancestor, is
        // OPTIONAL: the column can hold nulls, so each chunk must prove it
        // holds none.
        let nullable = column.max_def_level() > 0;

        let mut range: Option<(i64, i64)> = None;
        for rg in 0..self.metadata.num_row_groups() {
            let rgm = self.metadata.row_group(rg);
            if rgm.num_rows() == 0 {
                continue;
            }
            let stats = rgm.column(col_idx).statistics()?;
            if nullable && stats.null_count_opt() != Some(0) {
                return None;
            }
            let (rg_min, rg_max) = match stats {
                Statistics::Int32(s) => (i64::from(*s.min_opt()?), i64::from(*s.max_opt()?)),
                Statistics::Int64(s) => (*s.min_opt()?, *s.max_opt()?),
                _ => return None,
            };
            range = Some(range.map_or((rg_min, rg_max), |(lo, hi)| {
                (lo.min(rg_min), hi.max(rg_max))
            }));
        }
        // The caller reads `Some(v)` as "v on every row", so a column that
        // spans more than one value is refused, not folded: `{0, 1}` has max
        // 1 but says something about each row.
        let (min, max) = range?;
        (min == max).then_some(max)
    }

    /// The Arrow schema of the file (including the `level` column).
    pub fn schema(&self) -> &SchemaRef {
        &self.schema
    }

    /// The level materialization mode, resolving an absent `mode` to
    /// [`Mode::Partitioning`] per §3.4 (the safe reader default).
    pub fn mode(&self) -> Mode {
        self.meta.mode.unwrap_or(Mode::Partitioning)
    }

    /// The number of levels declared in the footer.
    pub fn num_levels(&self) -> usize {
        self.meta.levels.len()
    }

    /// The inclusive `[start, end]` row-group band that *belongs to* `level_idx`
    /// (mode-independent), per the §3.3 span rule. Level 0 starts at RG 0; level
    /// `k` starts at `levels[k-1].row_group_end + 1`.
    fn level_band(&self, level_idx: usize) -> Result<(usize, usize), ReaderError> {
        let levels = &self.meta.levels;
        if level_idx >= levels.len() {
            return Err(ReaderError::LevelOutOfRange {
                level: level_idx,
                num_levels: levels.len(),
            });
        }
        let start = if level_idx == 0 {
            0
        } else {
            (levels[level_idx - 1].row_group_end + 1) as usize
        };
        let end = levels[level_idx].row_group_end as usize;
        Ok((start, end))
    }

    /// The row groups a reader must fetch to render `level_idx` (spec §5.1):
    ///
    /// - `duplicating`: exactly that level's own band (levels are self-contained).
    /// - `partitioning`: the **prefix** `0..=end` (levels accumulate).
    pub fn row_groups_for_level(&self, level_idx: usize) -> Result<Vec<usize>, ReaderError> {
        let (start, end) = self.level_band(level_idx)?;
        let rgs = match self.mode() {
            Mode::Duplicating => (start..=end).collect(),
            Mode::Partitioning => (0..=end).collect(),
        };
        Ok(rgs)
    }

    /// Select the level for a target GSD (meters), per the §5.1 selection rule:
    /// the **finest** (highest-index) level whose `gsd >= target_gsd`. If the
    /// target is coarser than level 0 (no level qualifies), returns level 0; if
    /// finer than the finest level, all levels qualify so this returns `L-1`.
    pub fn level_for_gsd(&self, target_gsd: f64) -> usize {
        // `gsd` is strictly decreasing coarse→fine, so the qualifying set is a
        // prefix `0..=k`; the finest qualifying level is its last element.
        let mut selected = None;
        for (i, level) in self.meta.levels.iter().enumerate() {
            if level.gsd >= target_gsd {
                selected = Some(i);
            }
        }
        selected.unwrap_or(0)
    }

    /// Select the level for a Web Mercator target zoom `z`, mapping `z` to a
    /// target GSD via the §5.2 formula and applying [`Self::level_for_gsd`].
    pub fn level_for_zoom(&self, z: u8) -> usize {
        self.level_for_gsd(gsd(z))
    }

    /// Row groups (over the whole file) whose covering bbox statistics intersect
    /// `bbox` = `[xmin, ymin, xmax, ymax]`. Row groups with missing statistics are
    /// **kept conservatively** (they cannot be safely pruned).
    pub fn row_groups_intersecting_bbox(&self, bbox: &[f64; 4]) -> Vec<usize> {
        let bounds = extract_row_group_bounds_from_metadata(&self.metadata).unwrap_or_default();
        let filter = TileBounds {
            lng_min: bbox[0],
            lat_min: bbox[1],
            lng_max: bbox[2],
            lat_max: bbox[3],
        };
        (0..self.num_row_groups())
            .filter(|&i| match bounds.get(i) {
                Some(Some(b)) => b.intersects(&filter),
                // Missing stats (or short vec): keep conservatively.
                _ => true,
            })
            .collect()
    }

    /// The row groups actually read for `level_idx` given an optional viewport
    /// `bbox`: the level's RG set (§5.1) intersected with the bbox-pruned set.
    /// With `bbox == None` this is exactly [`Self::row_groups_for_level`].
    pub fn selected_row_groups(
        &self,
        level_idx: usize,
        bbox: Option<[f64; 4]>,
    ) -> Result<Vec<usize>, ReaderError> {
        let level_rgs = self.row_groups_for_level(level_idx)?;
        match bbox {
            None => Ok(level_rgs),
            Some(bb) => {
                let pruned: HashSet<usize> =
                    self.row_groups_intersecting_bbox(&bb).into_iter().collect();
                Ok(level_rgs
                    .into_iter()
                    .filter(|r| pruned.contains(r))
                    .collect())
            }
        }
    }

    /// Read a level, optionally pruned by a viewport `bbox`, as an iterator of
    /// `Result<RecordBatch, ArrowError>`. Reads **only** the selected row groups
    /// ([`Self::selected_row_groups`]).
    pub fn read_level(
        &self,
        level_idx: usize,
        bbox: Option<[f64; 4]>,
    ) -> Result<ParquetRecordBatchReader, ReaderError> {
        let selected = self.selected_row_groups(level_idx, bbox)?;
        let file = File::open(&self.path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?.with_row_groups(selected);
        // The builder's default batch (1024 rows), clamped like every read.
        let reader = builder.with_batch_size(self.batch_rows(1024)).build()?;
        Ok(reader)
    }

    /// [`Self::read_level`] with an explicit Arrow batch size (the builder
    /// default is 1024 rows). Larger batches amortize per-batch overhead for
    /// consumers that do parallel per-row work on each batch (PMTiles export).
    pub fn read_level_with_batch_size(
        &self,
        level_idx: usize,
        bbox: Option<[f64; 4]>,
        batch_size: usize,
    ) -> Result<ParquetRecordBatchReader, ReaderError> {
        let selected = self.selected_row_groups(level_idx, bbox)?;
        let file = File::open(&self.path)?;
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
            .with_row_groups(selected)
            .with_batch_size(self.batch_rows(batch_size))
            .build()?;
        Ok(reader)
    }

    /// Read exactly `level_idx`'s **own** row-group band (`start..=end`, the
    /// §3.3 span), unpruned and independent of mode, with an explicit Arrow
    /// batch size.
    ///
    /// Unlike [`Self::read_level_with_batch_size`] — which in partitioning mode
    /// reads the accumulating prefix `0..=end` — this reads only the band that
    /// *belongs to* the level. It is the unit of work for a single-read,
    /// fan-out scan of the whole file (issue #233): reading each band once
    /// (`Σ_j |band_j|` = every row group exactly once) instead of re-reading a
    /// coarse band from every finer level's prefix (`Σ_k |prefix_k|`).
    pub fn read_band_with_batch_size(
        &self,
        level_idx: usize,
        batch_size: usize,
    ) -> Result<ParquetRecordBatchReader, ReaderError> {
        Ok(self.band_builder(level_idx, batch_size)?.build()?)
    }

    /// [`Self::read_band_with_batch_size`] projected to the top-level columns
    /// `roots` (indexes into [`Self::schema`]); batches carry only those
    /// columns, in schema order, with their field metadata intact.
    ///
    /// For consumers that need a narrow slice of every row — the export's
    /// fan-out bbox scan reads only the geometry — so the property columns are
    /// neither read nor held in the batches in flight.
    pub(crate) fn read_band_projected(
        &self,
        level_idx: usize,
        batch_size: usize,
        roots: &[usize],
    ) -> Result<ParquetRecordBatchReader, ReaderError> {
        let builder = self.band_builder(level_idx, batch_size)?;
        let mask = ProjectionMask::roots(builder.parquet_schema(), roots.iter().copied());
        Ok(builder.with_projection(mask).build()?)
    }

    /// Read exactly the row groups `row_groups` (ascending, file order),
    /// projected to the top-level columns `roots`, with an explicit batch
    /// size. Batches may straddle row-group boundaries.
    ///
    /// For the export's up-front `--feature-id` check (#443), which reads one
    /// column of every row group its statistics cannot vouch for.
    pub(crate) fn read_row_groups_projected(
        &self,
        row_groups: Vec<usize>,
        batch_size: usize,
        roots: &[usize],
    ) -> Result<ParquetRecordBatchReader, ReaderError> {
        let file = File::open(&self.path)?;
        let builder = ParquetRecordBatchReaderBuilder::try_new(file)?
            .with_row_groups(row_groups)
            .with_batch_size(self.batch_rows(batch_size));
        let mask = ProjectionMask::roots(builder.parquet_schema(), roots.iter().copied());
        Ok(builder.with_projection(mask).build()?)
    }

    /// The parsed Parquet footer (row-group sizes and column statistics).
    pub(crate) fn parquet_metadata(&self) -> &ParquetMetaData {
        &self.metadata
    }

    /// A reader builder over `level_idx`'s own row-group band.
    fn band_builder(
        &self,
        level_idx: usize,
        batch_size: usize,
    ) -> Result<ParquetRecordBatchReaderBuilder<File>, ReaderError> {
        let (start, end) = self.level_band(level_idx)?;
        let file = File::open(&self.path)?;
        Ok(ParquetRecordBatchReaderBuilder::try_new(file)?
            .with_row_groups((start..=end).collect())
            .with_batch_size(self.batch_rows(batch_size)))
    }

    /// `requested` rows per batch, clamped to [`Self::max_batch_rows`]
    /// (#563).
    fn batch_rows(&self, requested: usize) -> usize {
        requested.clamp(1, self.max_batch_rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::writer::{
        LevelSpec, LevelWriteOutcome, OverviewWriter, OverviewWriterOptions,
    };
    use arrow_array::{ArrayRef, Int64Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};
    use geo::{Geometry, LineString, Point, Polygon};
    use geoarrow::array::GeometryBuilder;
    use geoarrow::datatypes::GeometryType;
    use geoarrow_array::GeoArrowArray;

    // --- fixture builders (mirror writer.rs test helpers) --------------------

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

    fn build_geometry_array(ids: &[i64]) -> geoarrow::array::GeometryArray {
        let geoms: Vec<Option<Geometry>> = ids.iter().map(|&id| Some(geom_for(id))).collect();
        let typ = GeometryType::new(Default::default());
        let mut builder = GeometryBuilder::new(typ).with_prefer_multi(false);
        builder.extend_from_iter(geoms.iter().map(|x| x.as_ref()));
        builder.finish()
    }

    fn geometry_field() -> Field {
        let arr = build_geometry_array(&[0]);
        arr.data_type().to_field("geometry", true)
    }

    fn source_schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            geometry_field(),
        ])
    }

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

    /// Write a fixture file. `level_ids[k]` are the feature ids in level `k`.
    fn write_fixture(
        path: &std::path::Path,
        mode: Mode,
        level_ids: &[Vec<i64>],
        max_rg_size: usize,
    ) -> OverviewsMeta {
        let schema = Arc::new(source_schema());
        let specs: Vec<LevelSpec> = (0..level_ids.len())
            .map(|k| {
                // coarse→fine: decreasing gsd. Use z = 2 + 2k for distinct gsds.
                let z = (2 + 2 * k) as u8;
                LevelSpec::new(gsd(z), Some(z))
            })
            .collect();
        let mut opts = OverviewWriterOptions::new(mode, specs);
        opts.max_row_group_size = max_rg_size;

        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        for (k, ids) in level_ids.iter().enumerate() {
            assert_eq!(
                writer
                    .write_level(
                        k,
                        Some(ids.len()),
                        std::iter::once(source_batch(&schema, ids)),
                    )
                    .unwrap(),
                LevelWriteOutcome::Written
            );
        }
        writer.finish().unwrap()
    }

    /// Read the `id` column across a level reader.
    fn read_ids(reader: ParquetRecordBatchReader) -> Vec<i64> {
        let mut out = Vec::new();
        for batch in reader {
            let batch = batch.unwrap();
            let idx = batch.schema().index_of("id").unwrap();
            let col = batch
                .column(idx)
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap();
            out.extend(col.values().iter().copied());
        }
        out.sort();
        out
    }

    // --- tests ---------------------------------------------------------------

    #[test]
    fn open_parses_meta_and_band_ranges() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // 3 levels, one RG each (default big rg size).
        let written = write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]],
            10_000,
        );

        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.meta(), &written);
        assert_eq!(reader.num_levels(), 3);
        assert_eq!(reader.num_row_groups(), 3);
        assert_eq!(reader.mode(), Mode::Duplicating);
        // Band ranges match the writer's declared row_group_end (0,1,2).
        assert_eq!(reader.meta().levels[0].row_group_end, 0);
        assert_eq!(reader.meta().levels[1].row_group_end, 1);
        assert_eq!(reader.meta().levels[2].row_group_end, 2);
    }

    #[test]
    fn finest_level_mean_row_bytes_reports_finest_band() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // 3 levels; the finest (level 2) carries 6 rows in its own band.
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]],
            10_000,
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();

        // The signal is present and positive — a real file always has geometry
        // bytes per row, so callers never fall back to the flat constant here.
        let mean = reader.finest_level_mean_row_bytes().unwrap();
        assert!(mean > 0, "finest-level mean row bytes must be positive");

        // It is computed over the *finest* band only: it equals that band's
        // summed uncompressed bytes divided by its row count, which we can
        // recompute independently from the same metadata.
        let (start, end) = reader.level_band(reader.num_levels() - 1).unwrap();
        let mut bytes = 0u64;
        let mut rows = 0i64;
        for rg in start..=end {
            let rgm = reader.metadata.row_group(rg);
            bytes += rgm.total_byte_size().max(0) as u64;
            rows += rgm.num_rows();
        }
        assert_eq!(mean, bytes / rows as u64);
    }

    #[test]
    fn open_missing_key_errors() {
        // A plain (non-overview) parquet file: write with parquet directly.
        use parquet::arrow::ArrowWriter;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("a", DataType::Int64, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![1i64]))])
                .unwrap();
        {
            let file = File::create(tmp.path()).unwrap();
            let mut w = ArrowWriter::try_new(file, schema, None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }
        let err = OverviewReader::open(tmp.path()).unwrap_err();
        assert!(matches!(err, ReaderError::MissingOverviewsKey));
    }

    #[test]
    fn duplicating_selects_single_band() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]],
            10_000,
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        // Each level is exactly its own single RG.
        assert_eq!(reader.row_groups_for_level(0).unwrap(), vec![0]);
        assert_eq!(reader.row_groups_for_level(1).unwrap(), vec![1]);
        assert_eq!(reader.row_groups_for_level(2).unwrap(), vec![2]);
    }

    #[test]
    fn partitioning_selects_prefix() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Partitioning,
            &[vec![0, 2], vec![1, 3], vec![4, 5]],
            10_000,
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.mode(), Mode::Partitioning);
        // Prefix accumulation 0..=end.
        assert_eq!(reader.row_groups_for_level(0).unwrap(), vec![0]);
        assert_eq!(reader.row_groups_for_level(1).unwrap(), vec![0, 1]);
        assert_eq!(reader.row_groups_for_level(2).unwrap(), vec![0, 1, 2]);
    }

    #[test]
    fn level_out_of_range_errors() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(tmp.path(), Mode::Duplicating, &[vec![0, 2]], 10_000);
        let reader = OverviewReader::open(tmp.path()).unwrap();
        let err = reader.row_groups_for_level(5).unwrap_err();
        assert!(matches!(
            err,
            ReaderError::LevelOutOfRange {
                level: 5,
                num_levels: 1
            }
        ));
    }

    #[test]
    fn level_for_gsd_selection_edges() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // gsds: level0 = gsd(2)=9783.94, level1 = gsd(4)=2445.98, level2 = gsd(6)=611.50
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]],
            10_000,
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();

        // Exact match: target == a level's gsd → that level.
        assert_eq!(reader.level_for_gsd(gsd(2)), 0);
        assert_eq!(reader.level_for_gsd(gsd(4)), 1);
        assert_eq!(reader.level_for_gsd(gsd(6)), 2);
        // Between level0 and level1 → coarser (finest with gsd >= target).
        assert_eq!(reader.level_for_gsd(5000.0), 0);
        // Coarser than coarsest → level 0.
        assert_eq!(reader.level_for_gsd(20_000.0), 0);
        // Finer than finest → finest level (L-1).
        assert_eq!(reader.level_for_gsd(100.0), 2);
    }

    #[test]
    fn level_for_zoom_selection_edges() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]],
            10_000,
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.level_for_zoom(2), 0);
        assert_eq!(reader.level_for_zoom(4), 1);
        assert_eq!(reader.level_for_zoom(6), 2);
        // z0 is coarser than level 0 → level 0.
        assert_eq!(reader.level_for_zoom(0), 0);
        // z9 finer than finest → finest level.
        assert_eq!(reader.level_for_zoom(9), 2);
    }

    #[test]
    fn read_level_returns_exactly_that_level_rows() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 2], vec![0, 1, 2, 3], vec![0, 1, 2, 3, 4, 5]],
            10_000,
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();

        assert_eq!(read_ids(reader.read_level(0, None).unwrap()), vec![0, 2]);
        assert_eq!(
            read_ids(reader.read_level(1, None).unwrap()),
            vec![0, 1, 2, 3]
        );
        assert_eq!(
            read_ids(reader.read_level(2, None).unwrap()),
            vec![0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn bbox_pruning_selects_only_intersecting_row_groups() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // level0: ids 0,1 near origin (1 RG). level1: ids 100..103, far away,
        // split into 2 RGs by max_rg_size=2 (RG1: 100,101; RG2: 102,103).
        write_fixture(
            tmp.path(),
            Mode::Duplicating,
            &[vec![0, 1], vec![100, 101, 102, 103]],
            2,
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.num_row_groups(), 3);
        // level1 band is RG {1, 2}.
        assert_eq!(reader.row_groups_for_level(1).unwrap(), vec![1, 2]);

        // A bbox around (100,100) intersects RG1 (ids 100,101) only, not RG2
        // (ids 102,103) nor RG0 (near origin).
        let bbox = [99.0, 99.0, 101.0, 101.0];
        let pruned = reader.row_groups_intersecting_bbox(&bbox);
        assert_eq!(pruned, vec![1], "whole-file pruned set");

        // Intersecting the level-1 band with the pruned set → {1}.
        let selected = reader.selected_row_groups(1, Some(bbox)).unwrap();
        assert_eq!(selected, vec![1]);

        // And reading returns only ids 100,101.
        let ids = read_ids(reader.read_level(1, Some(bbox)).unwrap());
        assert_eq!(ids, vec![100, 101]);
    }

    // --- int_column_max on foreign-written columns (#399) -------------------

    /// One-level overview fixture whose schema is `id`, `geometry`, then the
    /// given `field`, with `values` as that column's array. Written through
    /// the converter's own writer so the footer is valid; the column's Arrow
    /// type is whatever the caller passes (a pyarrow-style `UInt32`, a
    /// nullable `Int32`, ...), which the converter itself never emits.
    fn write_int_column_fixture(
        path: &std::path::Path,
        field: Field,
        values: ArrayRef,
    ) -> OverviewsMeta {
        let n = values.len();
        let ids: Vec<i64> = (0..n as i64).collect();
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            geometry_field(),
            field,
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(ids.clone())),
                Arc::new(build_geometry_array(&ids).to_array_ref()),
                values,
            ],
        )
        .unwrap();
        let opts =
            OverviewWriterOptions::new(Mode::Duplicating, vec![LevelSpec::new(gsd(2), Some(2))]);
        let mut writer = OverviewWriter::create(path, &schema, opts).unwrap();
        assert_eq!(
            writer
                .write_level(0, Some(n), std::iter::once(batch))
                .unwrap(),
            LevelWriteOutcome::Written
        );
        writer.finish().unwrap()
    }

    /// The signed INT32 NOT NULL column the converter writes, holding one
    /// value throughout: the statistic is exact and the fold is what the
    /// caller relies on.
    #[test]
    fn int_column_max_reads_signed_not_null_column() {
        use arrow_array::Int32Array;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_int_column_fixture(
            tmp.path(),
            Field::new("n", DataType::Int32, false),
            Arc::new(Int32Array::from(vec![3, 3, 3])),
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.int_column_max("n"), Some(3));
        assert_eq!(reader.int_column_max("absent"), None);
    }

    /// The caller reads `Some(1)` as "1 on every row", so a column whose
    /// values are not all the same must not fold at all: `{0, 1}` and
    /// `{-3, 1}` both have max 1 while carrying information (#399 review).
    /// The min is in the same chunk statistics, so the check is free.
    #[test]
    fn int_column_max_refuses_column_whose_min_differs_from_max() {
        use arrow_array::Int32Array;
        for values in [vec![0, 1], vec![-3, 1], vec![1, 3, 1]] {
            let tmp = tempfile::NamedTempFile::new().unwrap();
            write_int_column_fixture(
                tmp.path(),
                Field::new("n", DataType::Int32, false),
                Arc::new(Int32Array::from(values.clone())),
            );
            let reader = OverviewReader::open(tmp.path()).unwrap();
            assert_eq!(
                reader.int_column_max("n"),
                None,
                "{values:?} is not one value on every row"
            );
        }
    }

    /// A `UInt32` column over INT32 physical: a value >= 2^31 is a negative
    /// i32 when read as signed, so a naive fold would report a max *below*
    /// the real one (here: 1) and the caller would withhold a column that is
    /// plainly not 1 everywhere. Unsigned logical types are refused outright:
    /// whether the stored min/max were even computed with unsigned ordering
    /// depends on the writer, so the heuristic must not trust them.
    #[test]
    fn int_column_max_refuses_unsigned_int32_column() {
        use arrow_array::UInt32Array;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_int_column_fixture(
            tmp.path(),
            Field::new("n", DataType::UInt32, false),
            Arc::new(UInt32Array::from(vec![1u32, 1 << 31, 1])),
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(
            reader.int_column_max("n"),
            None,
            "an unsigned column must not be folded as signed"
        );
    }

    /// `UInt64` over INT64 physical: the same sign hazard, and a max that
    /// does not even fit `i64`.
    #[test]
    fn int_column_max_refuses_unsigned_int64_column() {
        use arrow_array::UInt64Array;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_int_column_fixture(
            tmp.path(),
            Field::new("n", DataType::UInt64, false),
            Arc::new(UInt64Array::from(vec![1u64, u64::MAX, 1])),
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.int_column_max("n"), None);
    }

    /// Even an unsigned column whose values all fit the signed range is
    /// refused: the caller needs "provably 1 everywhere", and the type alone
    /// says the statistic's ordering is writer-dependent.
    #[test]
    fn int_column_max_refuses_unsigned_column_even_when_small() {
        use arrow_array::UInt32Array;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_int_column_fixture(
            tmp.path(),
            Field::new("n", DataType::UInt32, false),
            Arc::new(UInt32Array::from(vec![1u32, 1, 1])),
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.int_column_max("n"), None);
    }

    /// A nullable column holding `{1, null}` has max 1, but it is not 1
    /// everywhere: the null/1 distinction is information the caller would
    /// lose. Any row group with `null_count > 0` makes the answer `None`.
    #[test]
    fn int_column_max_refuses_column_with_nulls() {
        use arrow_array::Int32Array;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_int_column_fixture(
            tmp.path(),
            Field::new("n", DataType::Int32, true),
            Arc::new(Int32Array::from(vec![Some(1), None, Some(1)])),
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.int_column_max("n"), None);
    }

    /// A column declared nullable but with no nulls written is fine: the
    /// statistic records `null_count = 0`, so the max is exact.
    #[test]
    fn int_column_max_accepts_nullable_column_without_nulls() {
        use arrow_array::Int32Array;
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_int_column_fixture(
            tmp.path(),
            Field::new("n", DataType::Int32, true),
            Arc::new(Int32Array::from(vec![Some(2), Some(2), Some(2)])),
        );
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert_eq!(reader.int_column_max("n"), Some(2));
    }

    /// A foreign writer that recorded no column statistics: there is no max
    /// to fold, so the answer is `None` (the caller then treats the column as
    /// carrying information).
    #[test]
    fn int_column_max_is_none_without_statistics() {
        use arrow_array::Int32Array;
        use parquet::arrow::ArrowWriter;
        use parquet::file::metadata::KeyValue;
        use parquet::file::properties::{EnabledStatistics, WriterProperties};

        // Borrow a valid one-level footer from the converter's own writer...
        let donor = tempfile::NamedTempFile::new().unwrap();
        let meta = write_int_column_fixture(
            donor.path(),
            Field::new("n", DataType::Int32, false),
            Arc::new(Int32Array::from(vec![1, 1])),
        );
        // ...and put it on a plain parquet file written with stats disabled,
        // one row group, matching the footer's single `row_group_end = 0`.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let schema = Arc::new(Schema::new(vec![Field::new("n", DataType::Int32, false)]));
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int32Array::from(vec![1, 1]))])
                .unwrap();
        let props = WriterProperties::builder()
            .set_statistics_enabled(EnabledStatistics::None)
            .set_key_value_metadata(Some(vec![KeyValue::new(
                OVERVIEWS_KEY.to_string(),
                meta.to_json().unwrap(),
            )]))
            .build();
        {
            let file = File::create(tmp.path()).unwrap();
            let mut w = ArrowWriter::try_new(file, schema, Some(props)).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
        }
        let reader = OverviewReader::open(tmp.path()).unwrap();
        assert!(
            reader
                .metadata
                .row_group(0)
                .column(0)
                .statistics()
                .is_none(),
            "fixture must carry no statistics"
        );
        assert_eq!(reader.int_column_max("n"), None);
    }
}
