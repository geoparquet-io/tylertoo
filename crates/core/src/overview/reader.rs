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

        Ok(Self {
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

    /// The total number of Parquet row groups in the file.
    pub fn num_row_groups(&self) -> usize {
        self.metadata.num_row_groups()
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
        let reader = ParquetRecordBatchReaderBuilder::try_new(file)?
            .with_row_groups(selected)
            .build()?;
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
            .with_batch_size(batch_size)
            .build()?;
        Ok(reader)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::writer::{LevelSpec, OverviewWriter, OverviewWriterOptions};
    use arrow_array::{Int64Array, RecordBatch, StringArray};
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
            writer
                .write_level(
                    k,
                    Some(ids.len()),
                    std::iter::once(source_batch(&schema, ids)),
                )
                .unwrap();
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
}
