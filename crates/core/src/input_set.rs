//! Multi-partition conversion input: [`ConvertSource`] (v0.7).
//!
//! Real-world GeoParquet datasets frequently arrive as a *set* of parquet
//! files (Hive/Spark `part-*.parquet` directories, Overture-style
//! partitions). This module generalizes the converter's single
//! [`InputSource`] into a [`ConvertSource`] that is either one file/object
//! or an ordered set of local partitions resolved from a directory or a
//! glob pattern.
//!
//! # The row-order invariant (load-bearing)
//!
//! The streaming converter reads its input **several times** (pass 1
//! assignment scan, buffered coarse levels, canonical finest level) and
//! keys its winner tables by **global row offset**. Every pass must
//! therefore see *the same rows in the same order*. A `ConvertSource`
//! guarantees this by construction:
//!
//! - partitions are sorted lexicographically at resolve time and never
//!   reordered;
//! - [`ConvertSource::open_stream`] concatenates the parts' batches in
//!   part order, part `i + 1` opening only after part `i` is exhausted;
//! - per-part row-group selections ([`RowGroupSelection`]) are computed
//!   once and applied identically on every open.
//!
//! # Compatibility validation
//!
//! All partitions must be mutually compatible ([`MultiSource`] validates
//! against partition 0 at construction): identical field names, types, and
//! order; identical field (extension) metadata — geometry encoding must
//! match; and an identical detected CRS. Nullability is the one permitted
//! difference: the exposed schema unions it (any-nullable ⇒ nullable).
//!
//! Remote *prefixes* (e.g. `s3://bucket/dataset/`) are rejected with a
//! clear error for now; remote prefix listing lands later in this release.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_schema::{Field, Schema, SchemaRef};
use parquet::arrow::arrow_reader::ParquetRecordBatchReader;
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::{KeyValue, ParquetMetaData};

use crate::input::{is_remote_scheme, url_scheme, FetchStats, InputError, InputSource};

/// A resolved conversion input: one parquet file/object, or an ordered set
/// of local parquet partitions read as one logical dataset.
#[derive(Debug)]
pub enum ConvertSource {
    /// A single parquet file or remote object (the historical input shape).
    Single(InputSource),
    /// An ordered, validated set of local parquet partitions.
    Multi(MultiSource),
}

/// Footer-derived metadata of one partition.
#[derive(Debug, Clone)]
struct PartMeta {
    /// Arrow schema decoded from the part's footer.
    schema: SchemaRef,
    /// Parsed parquet metadata (row groups, key-value metadata).
    parquet: Arc<ParquetMetaData>,
}

/// An ordered set of local parquet partitions with footers loaded and
/// validated up front (see the module docs for the compatibility rules).
#[derive(Debug)]
pub struct MultiSource {
    /// The original input string (directory path or glob pattern).
    root: String,
    /// The partitions, sorted lexicographically. Ordering is load-bearing:
    /// global row offsets are assigned in this order.
    parts: Vec<InputSource>,
    /// Per-part footer metadata, parallel to `parts`.
    metas: Vec<PartMeta>,
    /// The unioned schema: partition 0's fields with nullability OR-ed
    /// across all partitions.
    schema: SchemaRef,
}

/// Per-part row-group selection: the multi-file analogue of the single-file
/// `selected_row_groups: Option<&[usize]>` (bbox pruning, #102). Entry `i`
/// holds part `i`'s *local* row-group indices; an empty entry skips the
/// part entirely.
#[derive(Debug, Clone)]
pub struct RowGroupSelection(Vec<Vec<usize>>);

impl RowGroupSelection {
    /// Build from per-part local row-group index lists.
    pub fn from_parts(parts: Vec<Vec<usize>>) -> Self {
        RowGroupSelection(parts)
    }

    /// Per-part selections, in part order.
    pub fn parts(&self) -> &[Vec<usize>] {
        &self.0
    }

    /// Total number of selected row groups across all parts.
    pub fn total_selected(&self) -> usize {
        self.0.iter().map(Vec::len).sum()
    }
}

/// How to read a [`ConvertSource`]: batch size, optional root-column
/// projection (identical schemas make one index set valid for every part),
/// and optional per-part row-group selection.
#[derive(Debug, Clone, Copy)]
pub struct ReadPlan<'a> {
    /// Rows per record batch (clamped to >= 1).
    pub batch_size: usize,
    /// Root (top-level) column indices to read; `None` = all columns.
    pub projection: Option<&'a [usize]>,
    /// Per-part row-group selection; `None` = all row groups.
    pub row_groups: Option<&'a RowGroupSelection>,
}

/// Sequential record-batch stream over all parts of a [`ConvertSource`],
/// in part order. Part `i + 1`'s reader is opened lazily when part `i` is
/// exhausted; on each part transition the finished part's in-memory read
/// cache is released ([`InputSource::release_read_cache`]) so resident
/// memory stays bounded by one part's working set.
pub struct SourceStream<'a> {
    parts: &'a [InputSource],
    projection: Option<Vec<usize>>,
    row_groups: Option<Vec<Vec<usize>>>,
    batch_size: usize,
    part_idx: usize,
    current: Option<ParquetRecordBatchReader>,
    done: bool,
}

impl ConvertSource {
    /// Wrap an already-constructed [`InputSource`] (single file/object).
    pub fn single(source: InputSource) -> Self {
        ConvertSource::Single(source)
    }

    /// Resolve a CLI-style input string:
    ///
    /// - existing local file → `Single` (byte-identical behavior to today);
    /// - existing local directory → recursive `.parquet` collection
    ///   (sorted; `_`/`.`-prefixed basenames such as `_SUCCESS` skipped);
    /// - string containing glob metacharacters (`*?[`) → glob expansion,
    ///   filtered to `.parquet` files, sorted, deduplicated;
    /// - remote URL ending in `.parquet` → `Single` (unchanged);
    /// - remote prefix → [`InputError::RemotePrefixUnsupported`] (for now);
    /// - a single resolved partition collapses to `Single`;
    /// - an empty directory/glob result → [`InputError::NoParquetInputs`].
    pub fn resolve(input: &str) -> Result<Self, InputError> {
        if let Some(scheme) = url_scheme(input) {
            if !scheme.eq_ignore_ascii_case("file")
                && is_remote_scheme(scheme)
                && !remote_is_parquet_object(input)
            {
                return Err(InputError::RemotePrefixUnsupported {
                    url: input.to_string(),
                });
            }
            // `file://` maps to a local path, remote `.parquet` objects stay
            // single, unsupported schemes get the standard error — all
            // exactly as before.
            return Ok(ConvertSource::Single(InputSource::from_str_input(input)?));
        }
        let path = Path::new(input);
        if path.is_file() {
            // Existing local file: byte-identical behavior to today.
            return Ok(ConvertSource::Single(InputSource::Local(
                path.to_path_buf(),
            )));
        }
        if path.is_dir() {
            let files = list_parquet_files(path)?;
            return Self::from_local_files(input, files);
        }
        if has_glob_meta(input) {
            let files = expand_glob(input)?;
            return Self::from_local_files(input, files);
        }
        // Nonexistent plain path: keep today's behavior (the io error
        // surfaces at open time).
        Ok(ConvertSource::Single(InputSource::Local(
            path.to_path_buf(),
        )))
    }

    /// `Single` for one file, `Multi` for several, a clear error for none.
    fn from_local_files(input: &str, mut files: Vec<PathBuf>) -> Result<Self, InputError> {
        match files.len() {
            0 => Err(InputError::NoParquetInputs {
                input: input.to_string(),
            }),
            1 => Ok(ConvertSource::Single(InputSource::Local(files.remove(0)))),
            _ => Ok(ConvertSource::Multi(MultiSource::from_local_files(
                input.to_string(),
                files,
            )?)),
        }
    }

    /// [`ConvertSource::resolve`] for `Path` inputs (non-UTF-8 paths fall
    /// back to a single local source, as [`InputSource::from_path`] does).
    pub fn resolve_path(path: &Path) -> Result<Self, InputError> {
        match path.to_str() {
            Some(s) => Self::resolve(s),
            None => Ok(ConvertSource::Single(InputSource::from_path(path)?)),
        }
    }

    /// The underlying parts, in read order (a single source is one part).
    pub fn parts(&self) -> &[InputSource] {
        match self {
            ConvertSource::Single(s) => std::slice::from_ref(s),
            ConvertSource::Multi(m) => &m.parts,
        }
    }

    /// Whether any part is remote.
    pub fn is_remote(&self) -> bool {
        self.parts().iter().any(InputSource::is_remote)
    }

    /// Place the remote-input disk spill in `dir` for every part (#272).
    /// No-op for local parts, which never spill.
    pub fn set_spill_dir(&self, dir: Option<&Path>) {
        for part in self.parts() {
            part.set_spill_dir(dir);
        }
    }

    /// Human-readable input name: the path/URL for a single source,
    /// `"<root> (N partitions)"` for a multi source.
    pub fn display_name(&self) -> String {
        match self {
            ConvertSource::Single(s) => s.display_name(),
            ConvertSource::Multi(m) => {
                format!("{} ({} partitions)", m.root, m.parts.len())
            }
        }
    }

    /// The Arrow schema of the dataset. For a multi source this is the
    /// validated union schema (nullability OR-ed across parts).
    pub fn schema(&self) -> Result<SchemaRef, InputError> {
        match self {
            ConvertSource::Single(s) => Ok(load_part_meta(s)?.schema),
            ConvertSource::Multi(m) => Ok(m.schema.clone()),
        }
    }

    /// Parquet key-value metadata of partition 0 (the `geo` metadata used
    /// for CRS detection; construction validated all parts agree).
    pub fn key_value_metadata(&self) -> Result<Option<Vec<KeyValue>>, InputError> {
        match self {
            ConvertSource::Single(s) => Ok(load_part_meta(s)?
                .parquet
                .file_metadata()
                .key_value_metadata()
                .cloned()),
            ConvertSource::Multi(m) => Ok(m.metas[0]
                .parquet
                .file_metadata()
                .key_value_metadata()
                .cloned()),
        }
    }

    /// Total number of row groups across all parts.
    pub fn num_row_groups_total(&self) -> Result<usize, InputError> {
        Ok(self
            .metas()?
            .iter()
            .map(|m| m.parquet.num_row_groups())
            .sum())
    }

    /// Per-part bbox row-group selection (#102): applies the single-file
    /// covering-statistics pruning to each part independently.
    /// `bbox_units` is `[xmin, ymin, xmax, ymax]` in the file CRS units.
    pub fn select_row_groups(
        &self,
        bbox_units: &[f64; 4],
    ) -> Result<RowGroupSelection, InputError> {
        let per_part = self
            .metas()?
            .iter()
            .map(|m| crate::overview::convert::select_input_row_groups(&m.parquet, bbox_units))
            .collect();
        Ok(RowGroupSelection(per_part))
    }

    /// Total *compressed* bytes of the selected row groups (`None` = every
    /// row group of every part) — the projected disk-spill size the #272
    /// free-space preflight consults. Per-part sums come from the shared
    /// single-file helper [`crate::input::selected_compressed_bytes`].
    pub fn selected_input_bytes(
        &self,
        selection: Option<&RowGroupSelection>,
    ) -> Result<u64, InputError> {
        let metas = self.metas()?;
        if let Some(sel) = selection {
            debug_assert_eq!(metas.len(), sel.0.len());
        }
        Ok(metas
            .iter()
            .enumerate()
            .map(|(pi, m)| {
                crate::input::selected_compressed_bytes(
                    &m.parquet,
                    selection.map(|s| s.0[pi].as_slice()),
                )
            })
            .sum())
    }

    /// Fetch counters summed over remote parts (`None` when no part is
    /// remote). `object_size` is the summed size of the remote objects.
    pub fn fetch_stats(&self) -> Option<FetchStats> {
        let mut total: Option<FetchStats> = None;
        for part in self.parts() {
            if let Some(s) = part.fetch_stats() {
                let t = total.get_or_insert(FetchStats::default());
                t.requests += s.requests;
                t.bytes_fetched += s.bytes_fetched;
                t.object_size += s.object_size;
            }
        }
        total
    }

    /// Open a sequential batch stream over all parts (see [`SourceStream`]).
    pub fn open_stream(&self, plan: &ReadPlan<'_>) -> Result<SourceStream<'_>, InputError> {
        let parts = self.parts();
        if let Some(sel) = plan.row_groups {
            debug_assert_eq!(
                sel.0.len(),
                parts.len(),
                "row-group selection must cover every part"
            );
        }
        Ok(SourceStream {
            parts,
            projection: plan.projection.map(<[usize]>::to_vec),
            row_groups: plan.row_groups.map(|s| s.0.clone()),
            batch_size: plan.batch_size.max(1),
            part_idx: 0,
            current: None,
            done: false,
        })
    }

    /// Footer metadata per part: borrowed for `Multi` (loaded at
    /// construction), loaded on demand for `Single`.
    fn metas(&self) -> Result<std::borrow::Cow<'_, [PartMeta]>, InputError> {
        match self {
            ConvertSource::Single(s) => Ok(std::borrow::Cow::Owned(vec![load_part_meta(s)?])),
            ConvertSource::Multi(m) => Ok(std::borrow::Cow::Borrowed(&m.metas)),
        }
    }
}

/// Load one part's footer: Arrow schema + parsed parquet metadata. Cheap
/// for local files (OS page cache); remote sources reuse their cached
/// footer after the first open.
fn load_part_meta(source: &InputSource) -> Result<PartMeta, InputError> {
    let builder = source.open()?;
    Ok(PartMeta {
        schema: builder.schema().clone(),
        parquet: builder.metadata().clone(),
    })
}

impl MultiSource {
    /// Build and validate a multi source over local partition files.
    fn from_local_files(root: String, files: Vec<PathBuf>) -> Result<Self, InputError> {
        Self::from_sources(root, files.into_iter().map(InputSource::Local).collect())
    }

    /// Build a multi source over already-constructed parts: load every
    /// part's footer and validate compatibility against part 0 (see the
    /// module docs). `parts` must be non-empty and already ordered.
    pub fn from_sources(root: String, parts: Vec<InputSource>) -> Result<Self, InputError> {
        assert!(!parts.is_empty(), "MultiSource requires at least one part");
        let metas = parts
            .iter()
            .map(load_part_meta)
            .collect::<Result<Vec<_>, _>>()?;

        let first_name = parts[0].display_name();
        // The reference CRS: `Ok(crs)` per detect_crs_from_kv, `None` for an
        // unsupported/undetectable CRS. Parts must AGREE; supportedness
        // itself is enforced by the pipeline (against part 0), so a set
        // that consistently carries an unsupported CRS still errors with
        // the standard UnsupportedCrs message.
        let crs_of = |m: &PartMeta| {
            crate::overview::convert::detect_crs_from_kv(
                m.parquet.file_metadata().key_value_metadata(),
            )
            .ok()
        };
        let first_crs = crs_of(&metas[0]);

        for (part, meta) in parts.iter().zip(&metas).skip(1) {
            let incompatible = |detail: String| InputError::IncompatiblePartition {
                first: first_name.clone(),
                offender: part.display_name(),
                detail,
            };
            validate_schema_shape(&metas[0].schema, &meta.schema).map_err(&incompatible)?;
            let crs = crs_of(meta);
            if crs != first_crs {
                return Err(incompatible(format!(
                    "CRS mismatch: partition detects {crs:?} but the first partition \
                     detects {first_crs:?} (all partitions must share one CRS)"
                )));
            }
        }

        let schema = union_schema(&metas);
        Ok(MultiSource {
            root,
            parts,
            metas,
            schema,
        })
    }
}

/// Validate that `other` matches `first` in field count, names, types,
/// order, and field (extension) metadata — everything except nullability,
/// which the union schema absorbs. Returns a human-readable detail on the
/// first difference.
fn validate_schema_shape(first: &Schema, other: &Schema) -> Result<(), String> {
    if first.fields().len() != other.fields().len() {
        return Err(format!(
            "column count differs: {} vs {} in the first partition",
            other.fields().len(),
            first.fields().len()
        ));
    }
    for (ci, (f0, fi)) in first.fields().iter().zip(other.fields()).enumerate() {
        if f0.name() != fi.name() {
            return Err(format!(
                "column {ci} is named {:?} but the first partition has {:?} \
                 (columns must match in name and order)",
                fi.name(),
                f0.name()
            ));
        }
        if f0.data_type() != fi.data_type() {
            return Err(format!(
                "column {:?} has type {:?} but the first partition has {:?}",
                fi.name(),
                fi.data_type(),
                f0.data_type()
            ));
        }
        if f0.metadata() != fi.metadata() {
            return Err(format!(
                "column {:?} carries different field (extension) metadata than \
                 the first partition — geometry encoding/CRS metadata must match",
                fi.name()
            ));
        }
    }
    Ok(())
}

/// Partition 0's schema with nullability OR-ed across all parts
/// (any-nullable ⇒ nullable); schema-level metadata from partition 0.
fn union_schema(metas: &[PartMeta]) -> SchemaRef {
    let first = &metas[0].schema;
    let fields: Vec<Field> = first
        .fields()
        .iter()
        .enumerate()
        .map(|(ci, f0)| {
            let nullable = metas.iter().any(|m| m.schema.field(ci).is_nullable());
            f0.as_ref().clone().with_nullable(nullable)
        })
        .collect();
    Arc::new(Schema::new_with_metadata(fields, first.metadata().clone()))
}

impl SourceStream<'_> {
    /// Open part `i`'s reader with this stream's projection / row-group
    /// selection / batch size. Schemas are identical across parts, so the
    /// root-column projection indices are valid for every part.
    fn open_part(&self, i: usize) -> Result<ParquetRecordBatchReader, InputError> {
        let mut builder = self.parts[i].open()?;
        if let Some(cols) = &self.projection {
            let mask = ProjectionMask::roots(builder.parquet_schema(), cols.iter().copied());
            builder = builder.with_projection(mask);
        }
        if let Some(sel) = &self.row_groups {
            builder = builder.with_row_groups(sel[i].clone());
        }
        Ok(builder.with_batch_size(self.batch_size).build()?)
    }
}

impl Iterator for SourceStream<'_> {
    type Item = Result<RecordBatch, InputError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            if let Some(reader) = &mut self.current {
                match reader.next() {
                    Some(Ok(batch)) => return Some(Ok(batch)),
                    Some(Err(e)) => {
                        self.done = true;
                        return Some(Err(InputError::Arrow(e)));
                    }
                    None => {
                        // Part exhausted. Release its in-memory read cache
                        // before moving on (bounds resident memory to one
                        // part's working set); a single-part stream never
                        // releases, preserving the single-file multi-pass
                        // cache behavior.
                        self.current = None;
                        if self.part_idx + 1 < self.parts.len() {
                            self.parts[self.part_idx].release_read_cache();
                        }
                        self.part_idx += 1;
                    }
                }
                continue;
            }
            if self.part_idx >= self.parts.len() {
                self.done = true;
                return None;
            }
            // Skip parts whose row-group selection is empty without opening
            // them at all (a bbox-pruned remote part is never touched).
            if let Some(sel) = &self.row_groups {
                if sel[self.part_idx].is_empty() {
                    self.part_idx += 1;
                    continue;
                }
            }
            match self.open_part(self.part_idx) {
                Ok(reader) => self.current = Some(reader),
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
            }
        }
    }
}

/// Whether `input` contains glob metacharacters (`*`, `?`, `[`).
fn has_glob_meta(input: &str) -> bool {
    input.contains(['*', '?', '['])
}

/// Whether a remote URL names a single `.parquet` object (query string and
/// fragment ignored; trailing `/` means prefix).
fn remote_is_parquet_object(url: &str) -> bool {
    let no_fragment = url.split('#').next().unwrap_or(url);
    let no_query = no_fragment.split('?').next().unwrap_or(no_fragment);
    !no_query.ends_with('/') && no_query.to_ascii_lowercase().ends_with(".parquet")
}

/// Recursively collect `.parquet` files under `dir`, sorted
/// lexicographically. Basenames starting with `.` or `_` (files *and*
/// directories — `_SUCCESS`, `.crc`, `_temporary/`) are skipped.
pub(crate) fn list_parquet_files(dir: &Path) -> Result<Vec<PathBuf>, InputError> {
    fn hidden(path: &Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with('.') || n.starts_with('_'))
    }
    fn collect(dir: &Path, files: &mut Vec<PathBuf>) -> Result<(), InputError> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if hidden(&path) {
                continue;
            }
            if path.is_dir() {
                collect(&path, files)?;
            } else if path.extension().is_some_and(|ext| ext == "parquet") {
                files.push(path);
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    collect(dir, &mut files)?;
    files.sort();
    Ok(files)
}

/// Expand a glob pattern to `.parquet` files: matched files filtered by
/// extension, sorted lexicographically, deduplicated.
pub(crate) fn expand_glob(pattern: &str) -> Result<Vec<PathBuf>, InputError> {
    let paths = glob::glob(pattern).map_err(|e| InputError::GlobPattern {
        pattern: pattern.to_string(),
        message: e.to_string(),
    })?;
    let mut files = Vec::new();
    for entry in paths {
        let path = entry.map_err(|e| InputError::Io(e.into_error()))?;
        if path.is_file() && path.extension().is_some_and(|ext| ext == "parquet") {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::DataType;
    use parquet::arrow::ArrowWriter;
    use parquet::file::properties::WriterProperties;
    use std::fs::File;

    /// Write a small parquet file with the given fields/columns and row
    /// group size (None = single row group).
    fn write_parquet(
        path: &Path,
        fields: Vec<Field>,
        columns: Vec<ArrayRef>,
        max_row_group_size: Option<usize>,
    ) {
        let schema = Arc::new(Schema::new(fields));
        let batch = RecordBatch::try_new(schema.clone(), columns).unwrap();
        let props = max_row_group_size.map(|n| {
            WriterProperties::builder()
                .set_max_row_group_row_count(Some(n))
                .build()
        });
        let file = File::create(path).unwrap();
        let mut writer = ArrowWriter::try_new(file, schema, props).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    /// Standard two-column fixture: `id: Int64 (non-null)`, `name: Utf8`.
    fn write_standard(path: &Path, ids: Vec<i64>, nullable_id: bool) {
        let n = ids.len();
        write_parquet(
            path,
            vec![
                Field::new("id", DataType::Int64, nullable_id),
                Field::new("name", DataType::Utf8, true),
            ],
            vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(
                    (0..n).map(|i| format!("r{i}")).collect::<Vec<_>>(),
                )),
            ],
            None,
        );
    }

    fn tmpdir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    // --- resolution ---------------------------------------------------------

    #[test]
    fn resolve_existing_file_is_single() {
        let dir = tmpdir();
        let f = dir.path().join("a.parquet");
        write_standard(&f, vec![1, 2], false);
        let src = ConvertSource::resolve(f.to_str().unwrap()).unwrap();
        assert!(matches!(src, ConvertSource::Single(_)));
        assert_eq!(src.display_name(), f.display().to_string());
        assert_eq!(src.parts().len(), 1);
    }

    #[test]
    fn resolve_directory_recursive_sorted_multi() {
        let dir = tmpdir();
        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        write_standard(&dir.path().join("b.parquet"), vec![1], false);
        write_standard(&sub.join("a.parquet"), vec![2], false);
        std::fs::write(dir.path().join("readme.txt"), "x").unwrap();

        let src = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();
        let ConvertSource::Multi(m) = &src else {
            panic!("directory with 2 parquet files must resolve to Multi");
        };
        assert_eq!(m.parts.len(), 2);
        // Lexicographic: "b.parquet" < "sub/a.parquet".
        assert!(src.parts()[0].display_name().ends_with("b.parquet"));
        assert!(src.parts()[1].display_name().ends_with("a.parquet"));
        assert!(src.display_name().contains("(2 partitions)"));
    }

    #[test]
    fn list_parquet_files_skips_hidden_and_success_markers() {
        let dir = tmpdir();
        write_standard(&dir.path().join("part-0.parquet"), vec![1], false);
        write_standard(&dir.path().join("part-1.parquet"), vec![2], false);
        // Markers and hidden files/dirs must be skipped.
        std::fs::write(dir.path().join("_SUCCESS"), "").unwrap();
        write_standard(&dir.path().join("_stale.parquet"), vec![9], false);
        write_standard(&dir.path().join(".hidden.parquet"), vec![9], false);
        let hidden_dir = dir.path().join("_temporary");
        std::fs::create_dir(&hidden_dir).unwrap();
        write_standard(&hidden_dir.join("x.parquet"), vec![9], false);

        let files = list_parquet_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2, "only visible .parquet files: {files:?}");
        assert!(files.windows(2).all(|w| w[0] <= w[1]), "sorted: {files:?}");
        assert!(files[0].ends_with("part-0.parquet"));
        assert!(files[1].ends_with("part-1.parquet"));
    }

    #[test]
    fn resolve_empty_directory_errors_naming_input() {
        let dir = tmpdir();
        std::fs::write(dir.path().join("_SUCCESS"), "").unwrap();
        let err = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap_err();
        match err {
            InputError::NoParquetInputs { input } => {
                assert_eq!(input, dir.path().to_str().unwrap());
            }
            other => panic!("expected NoParquetInputs, got {other:?}"),
        }
    }

    #[test]
    fn resolve_glob_sorted_dedup_and_single_collapse() {
        let dir = tmpdir();
        write_standard(&dir.path().join("p2.parquet"), vec![1], false);
        write_standard(&dir.path().join("p1.parquet"), vec![2], false);
        std::fs::write(dir.path().join("p3.txt"), "x").unwrap();

        let pattern = format!("{}/p*.parquet", dir.path().display());
        let files = expand_glob(&pattern).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("p1.parquet"));
        assert!(files[1].ends_with("p2.parquet"));

        let src = ConvertSource::resolve(&pattern).unwrap();
        assert!(matches!(src, ConvertSource::Multi(_)));

        // A single-match glob collapses to Single.
        let single = format!("{}/p1*.parquet", dir.path().display());
        let src = ConvertSource::resolve(&single).unwrap();
        assert!(matches!(src, ConvertSource::Single(_)));

        // A no-match glob errors, naming the pattern.
        let none = format!("{}/zzz*.parquet", dir.path().display());
        let err = ConvertSource::resolve(&none).unwrap_err();
        assert!(matches!(err, InputError::NoParquetInputs { .. }), "{err:?}");
    }

    #[test]
    fn resolve_remote_prefix_rejected() {
        for url in [
            "s3://bucket/dataset/",
            "s3://bucket/dataset",
            "https://example.com/data/",
            "gs://bucket/prefix",
        ] {
            let err = ConvertSource::resolve(url).unwrap_err();
            assert!(
                matches!(err, InputError::RemotePrefixUnsupported { .. }),
                "{url} → {err:?}"
            );
        }
    }

    // --- compatibility validation -------------------------------------------

    #[test]
    fn schema_mismatch_names_offending_partition() {
        let dir = tmpdir();
        write_standard(&dir.path().join("a.parquet"), vec![1], false);
        // Different column name in the second partition.
        write_parquet(
            &dir.path().join("b.parquet"),
            vec![
                Field::new("id2", DataType::Int64, false),
                Field::new("name", DataType::Utf8, true),
            ],
            vec![
                Arc::new(Int64Array::from(vec![1i64])),
                Arc::new(StringArray::from(vec!["x"])),
            ],
            None,
        );
        let err = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap_err();
        match err {
            InputError::IncompatiblePartition {
                first,
                offender,
                detail,
            } => {
                assert!(first.ends_with("a.parquet"), "first: {first}");
                assert!(offender.ends_with("b.parquet"), "offender: {offender}");
                assert!(detail.contains("id2") || detail.contains("id"), "{detail}");
            }
            other => panic!("expected IncompatiblePartition, got {other:?}"),
        }
    }

    #[test]
    fn type_mismatch_rejected() {
        let dir = tmpdir();
        write_standard(&dir.path().join("a.parquet"), vec![1], false);
        write_parquet(
            &dir.path().join("b.parquet"),
            vec![
                Field::new("id", DataType::Utf8, false), // Int64 in part a
                Field::new("name", DataType::Utf8, true),
            ],
            vec![
                Arc::new(StringArray::from(vec!["1"])),
                Arc::new(StringArray::from(vec!["x"])),
            ],
            None,
        );
        let err = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap_err();
        assert!(
            matches!(err, InputError::IncompatiblePartition { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn nullability_difference_unions_to_nullable() {
        let dir = tmpdir();
        write_standard(&dir.path().join("a.parquet"), vec![1], false); // id non-null
        write_standard(&dir.path().join("b.parquet"), vec![2], true); // id nullable
        let src = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();
        let schema = src.schema().unwrap();
        let id = schema.field_with_name("id").unwrap();
        assert!(
            id.is_nullable(),
            "any-nullable must union to nullable: {schema:?}"
        );
    }

    // --- streaming ------------------------------------------------------------

    /// Row `id`s seen across the whole stream, in order.
    fn stream_ids(src: &ConvertSource, plan: &ReadPlan<'_>) -> Vec<i64> {
        let mut out = Vec::new();
        for batch in src.open_stream(plan).unwrap() {
            let batch = batch.unwrap();
            let ids = batch
                .column(batch.schema().index_of("id").unwrap())
                .as_any()
                .downcast_ref::<Int64Array>()
                .unwrap()
                .clone();
            out.extend(ids.values().iter().copied());
        }
        out
    }

    #[test]
    fn stream_concatenates_parts_in_order() {
        let dir = tmpdir();
        write_standard(&dir.path().join("p0.parquet"), vec![0, 1, 2], false);
        write_standard(&dir.path().join("p1.parquet"), vec![3, 4], false);
        write_standard(&dir.path().join("p2.parquet"), vec![5], false);
        let src = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();
        let plan = ReadPlan {
            batch_size: 2,
            projection: None,
            row_groups: None,
        };
        assert_eq!(stream_ids(&src, &plan), vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn stream_skips_zero_row_partition() {
        let dir = tmpdir();
        write_standard(&dir.path().join("p0.parquet"), vec![0, 1], false);
        write_standard(&dir.path().join("p1.parquet"), vec![], false); // 0 rows
        write_standard(&dir.path().join("p2.parquet"), vec![2, 3], false);
        let src = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();
        let plan = ReadPlan {
            batch_size: 1024,
            projection: None,
            row_groups: None,
        };
        assert_eq!(stream_ids(&src, &plan), vec![0, 1, 2, 3]);
    }

    #[test]
    fn stream_honors_per_part_row_group_selection() {
        let dir = tmpdir();
        // Two row groups per part (max_row_group_size = 2, 4 rows each).
        let f0 = dir.path().join("p0.parquet");
        let f1 = dir.path().join("p1.parquet");
        for (f, base) in [(&f0, 0i64), (&f1, 10i64)] {
            write_parquet(
                f,
                vec![Field::new("id", DataType::Int64, false)],
                vec![Arc::new(Int64Array::from(vec![
                    base,
                    base + 1,
                    base + 2,
                    base + 3,
                ]))],
                Some(2),
            );
        }
        let src = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(src.num_row_groups_total().unwrap(), 4);

        // Part 0: skip entirely (empty selection); part 1: second group only.
        let sel = RowGroupSelection::from_parts(vec![vec![], vec![1]]);
        assert_eq!(sel.total_selected(), 1);
        let plan = ReadPlan {
            batch_size: 1024,
            projection: None,
            row_groups: Some(&sel),
        };
        assert_eq!(stream_ids(&src, &plan), vec![12, 13]);

        // selected_input_bytes = the one selected group's compressed size;
        // None means every row group of every part.
        let bytes = src.selected_input_bytes(Some(&sel)).unwrap();
        assert!(bytes > 0);
        let all = RowGroupSelection::from_parts(vec![vec![0, 1], vec![0, 1]]);
        let all_bytes = src.selected_input_bytes(Some(&all)).unwrap();
        assert!(all_bytes > bytes);
        assert_eq!(src.selected_input_bytes(None).unwrap(), all_bytes);
    }

    #[test]
    fn stream_projection_applies_to_every_part() {
        let dir = tmpdir();
        write_standard(&dir.path().join("p0.parquet"), vec![0], false);
        write_standard(&dir.path().join("p1.parquet"), vec![1], false);
        let src = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();
        let cols = [0usize]; // id only
        let plan = ReadPlan {
            batch_size: 8,
            projection: Some(&cols),
            row_groups: None,
        };
        for batch in src.open_stream(&plan).unwrap() {
            let batch = batch.unwrap();
            assert_eq!(batch.num_columns(), 1);
            assert_eq!(batch.schema().field(0).name(), "id");
        }
    }

    // --- Send (the pipeline moves streams into reader threads) ---------------

    #[test]
    fn source_stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<SourceStream<'static>>();
        // Scoped reader threads capture &ConvertSource.
        fn assert_sync<T: Sync>() {}
        assert_sync::<ConvertSource>();
    }
}
