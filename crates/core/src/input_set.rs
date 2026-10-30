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
//! `s3://` / `gs://` *prefixes* (e.g. `s3://bucket/dataset/`) are listed
//! natively (sorted `.parquet` keys, one shared store instance);
//! `http(s)://` prefixes have no generic listing API and error with a
//! pointer at `--files-from`, which accepts an explicit ordered manifest
//! of files/URLs instead.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow_array::RecordBatch;
use arrow_schema::{Field, Schema, SchemaRef};
use tylertoo_io::ReaderMetadata;
use tylertoo_io::{KeyValue, ParquetMetaData};

#[cfg(feature = "remote")]
use crate::input::is_remote_scheme;
use crate::input::{url_scheme, FetchStats, InputError, InputSource};

/// A resolved conversion input: one parquet file/object, or an ordered set
/// of local parquet partitions read as one logical dataset.
#[derive(Debug)]
pub enum ConvertSource {
    /// A single parquet file or remote object (the historical input shape).
    Single(SingleSource),
    /// An ordered, validated set of local parquet partitions.
    Multi(MultiSource),
}

/// One parquet file/object plus its lazily cached footer metadata.
///
/// The cache means the header phase's accessors (schema, kv metadata,
/// row-group counts, selection) parse the footer ONCE — matching the pre-v0.7
/// single-open header cost.
#[derive(Debug)]
#[expect(
    clippy::disallowed_types,
    reason = "#779: per-operation caches, worker coordination and profiling; no diagnostic globals"
)]
pub struct SingleSource {
    source: InputSource,
    meta: std::sync::OnceLock<PartMeta>,
    /// Root columns every read is restricted to (#386); `None` = all.
    projection: std::sync::OnceLock<Vec<usize>>,
}

impl SingleSource {
    #[expect(
        clippy::disallowed_types,
        reason = "#779: per-operation caches, worker coordination and profiling; no diagnostic globals"
    )]
    fn new(source: InputSource) -> Self {
        SingleSource {
            source,
            meta: std::sync::OnceLock::new(),
            projection: std::sync::OnceLock::new(),
        }
    }

    /// The wrapped input.
    pub fn input(&self) -> &InputSource {
        &self.source
    }

    /// Footer metadata, parsed on first access and cached.
    fn meta(&self) -> Result<&PartMeta, InputError> {
        if let Some(m) = self.meta.get() {
            return Ok(m);
        }
        let m = load_part_meta(&self.source)?;
        // A concurrent load may have won the race; either copy is
        // equivalent (same footer).
        Ok(self.meta.get_or_init(|| m))
    }
}

/// Footer-derived metadata of one partition.
#[derive(Debug, Clone)]
struct PartMeta {
    resolved: Arc<crate::resolved_schema::ResolvedSchema>,
    /// Arrow schema decoded from the part's footer.
    schema: SchemaRef,
    /// Parsed parquet metadata (row groups, key-value metadata).
    parquet: Arc<ParquetMetaData>,
    /// The same footer in the shape the arrow reader wants, so opening a
    /// reader over this part never re-parses it ([`InputSource::open_with_metadata`]).
    /// Load-bearing for the pass-2 parallel read (#494), which opens one
    /// reader per segment: a 1084-segment run parsed the footer 1084 times,
    /// twice per conversion.
    reader: ReaderMetadata,
}

/// An ordered set of parquet partitions (local files and/or remote
/// objects) with footers loaded and validated up front (see the module
/// docs for the compatibility rules).
#[derive(Debug)]
#[expect(
    clippy::disallowed_types,
    reason = "#779: per-operation caches, worker coordination and profiling; no diagnostic globals"
)]
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
    /// Root columns every read is restricted to (#386); `None` = all.
    projection: std::sync::OnceLock<Vec<usize>>,
}

/// A pass-0 staging plan (#788, [`ConvertSource::staging_plan`]): one
/// [`PartStaging`] per part, in part order (empty for a local part).
#[derive(Debug)]
pub(crate) struct StagingPlan {
    parts: Vec<crate::input::PartStaging>,
}

impl StagingPlan {
    /// See [`ConvertSource::bytes_to_stage`].
    pub(crate) fn bytes_to_stage(&self) -> u64 {
        self.parts
            .iter()
            .map(crate::input::PartStaging::bytes_to_stage)
            .fold(0, u64::saturating_add)
    }
}

/// Per-part row-group selection.
///
/// This is the multi-file analogue of the single-file
/// `selected_row_groups: Option<&[usize]>` (bbox pruning, #102). Entry `i`
/// holds part `i`'s *local* row-group indices; an empty entry skips the
/// part entirely.
#[derive(Debug, Clone)]
pub struct RowGroupSelection(Vec<Vec<usize>>);

/// Maps a row's position in a [`ConvertSource`]'s read stream back to the
/// part and the row within that part's file (#553).
///
/// The streaming scan numbers features by their position in the stream it
/// actually reads: the selected row groups of every part, concatenated in
/// read order. That is the right key for the winner tables, and the wrong
/// thing to show a person: once `--bbox`/`--filter`/`--shard` prunes a row
/// group, or the input has more than one part, stream position 1041 is not
/// row 1041 of any file. This rebuilds the file row from footer row counts
/// alone, in the same part-then-selected-row-group order
/// [`ConvertSource::open_stream`] reads.
#[derive(Debug, Clone, Default)]
pub(crate) struct StreamRowLocator {
    /// One entry per selected, non-empty row group, in read order:
    /// `(first stream row, part, first file row within the part)`.
    starts: Vec<(u64, usize, u64)>,
    part_bases: Vec<u64>,
    /// Rows in the whole stream: one past the last stream row.
    total: u64,
}

impl StreamRowLocator {
    /// Build from per-part, per-row-group row counts
    /// ([`ConvertSource::part_row_group_row_counts`]) and the selection the
    /// stream reads (`None` = every row group of every part).
    pub(crate) fn new(row_counts: &[Vec<i64>], selected: Option<&RowGroupSelection>) -> Self {
        let mut starts = Vec::new();
        let mut stream_row = 0u64;
        let mut part_bases = Vec::with_capacity(row_counts.len());
        let mut part_base = 0u64;
        for (part, counts) in row_counts.iter().enumerate() {
            part_bases.push(part_base);
            // A row group's first file row: the sum of every earlier group's
            // rows, whether selected or not.
            let mut first_rows = Vec::with_capacity(counts.len());
            let mut acc = 0u64;
            for &n in counts {
                first_rows.push(acc);
                acc += n.max(0) as u64;
            }
            part_base += acc;
            let picked: Vec<usize> = match selected.and_then(|s| s.parts().get(part)) {
                Some(p) => p.clone(),
                None => (0..counts.len()).collect(),
            };
            for g in picked {
                let (Some(&n), Some(&first)) = (counts.get(g), first_rows.get(g)) else {
                    continue;
                };
                let n = n.max(0) as u64;
                if n == 0 {
                    continue;
                }
                starts.push((stream_row, part, first));
                stream_row += n;
            }
        }
        Self {
            starts,
            part_bases,
            total: stream_row,
        }
    }

    /// Original row ordinal across all parts, including pruned groups.
    pub(crate) fn global_row(&self, stream_row: usize) -> Option<u64> {
        let (part, row) = self.locate(stream_row)?;
        Some(self.part_bases[part] + row as u64)
    }

    /// Half-open original row ranges constituting the selected read stream.
    pub(crate) fn ranges(&self) -> Vec<(u64, u64)> {
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        for (i, &(stream, part, first)) in self.starts.iter().enumerate() {
            let end = self.starts.get(i + 1).map_or(self.total, |r| r.0);
            let first = self.part_bases[part] + first;
            let end = first + end - stream;
            if let Some(previous) = ranges.last_mut().filter(|r| r.1 == first) {
                previous.1 = end;
            } else {
                ranges.push((first, end));
            }
        }
        ranges
    }

    pub(crate) fn len(&self) -> u64 {
        self.total
    }

    /// `(part, row within the part's file)` for stream position
    /// `stream_row`, or `None` past the end of the stream.
    pub(crate) fn locate(&self, stream_row: usize) -> Option<(usize, usize)> {
        let r = stream_row as u64;
        if r >= self.total {
            return None;
        }
        // The last group starting at or before `r` holds it.
        let i = self.starts.partition_point(|&(start, _, _)| start <= r);
        let &(start, part, first) = self.starts.get(i.checked_sub(1)?)?;
        Some((part, (first + (r - start)) as usize))
    }
}

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

    /// Per-part intersection with `other` (#315): a row group survives only
    /// when both selections keep it, so the bbox covering pruning and the
    /// attribute-filter statistics pruning compose. Both selections must
    /// come from the same source (same part count and order).
    pub fn intersect(&self, other: &RowGroupSelection) -> RowGroupSelection {
        debug_assert_eq!(self.0.len(), other.0.len());
        let parts = self
            .0
            .iter()
            .zip(&other.0)
            .map(|(a, b)| a.iter().copied().filter(|i| b.contains(i)).collect())
            .collect();
        RowGroupSelection(parts)
    }
}

/// One reader worker's unit of work: a contiguous run of ONE part's selected
/// row groups (#494).
///
/// A run never straddles a part boundary, so parallel readers can be merged
/// back into the exact order [`SourceStream`] would have produced: parts stay
/// sequential, and within a part the runs are ascending and disjoint.
#[derive(Debug, Clone)]
pub(crate) struct ReadSegment {
    /// Index into [`ConvertSource::parts`].
    pub(crate) part: usize,
    /// The part's LOCAL row-group indices, ascending.
    pub(crate) row_groups: Vec<usize>,
    /// Rows the run holds, per the footer. Sizing only — the merge counts the
    /// rows it actually sees.
    pub(crate) rows: usize,
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

/// Sequential record-batch stream over all parts of a [`ConvertSource`], in part order.
///
/// Part `i + 1`'s reader is opened lazily when part `i` is
/// exhausted; on each part transition the finished part's in-memory read
/// cache is released ([`InputSource::release_read_cache`]) so resident
/// memory stays bounded by one part's working set.
pub struct SourceStream<'a> {
    parts: &'a [InputSource],
    /// Each part's already-parsed footer, parallel to `parts`, so opening a
    /// part's reader costs a `File::open` and nothing else (#494 follow-up).
    metas: Vec<ReaderMetadata>,
    projection: Option<Vec<usize>>,
    row_groups: Option<Vec<Vec<usize>>>,
    batch_size: usize,
    part_idx: usize,
    current: Option<tylertoo_io::UntrustedBatchReader>,
    done: bool,
}

impl ConvertSource {
    /// Wrap an already-constructed [`InputSource`] (single file/object).
    pub fn single(source: InputSource) -> Self {
        ConvertSource::Single(SingleSource::new(source))
    }

    /// Resolve a CLI-style input string:
    ///
    /// - existing local file → `Single` (byte-identical behavior to today);
    /// - existing local directory → recursive `.parquet` collection
    ///   (sorted; `_`/`.`-prefixed basenames such as `_SUCCESS` skipped);
    /// - string containing glob metacharacters (`*?[`) → glob expansion,
    ///   filtered to `.parquet` files, sorted, deduplicated;
    /// - remote single-object URL (no trailing slash — including
    ///   extension-less presigned/API URLs) → `Single` (unchanged);
    /// - `s3://` / `gs://` prefix (path ending `/`) → native object
    ///   listing: `.parquet` keys sorted by key, `_SUCCESS`/zero-byte/
    ///   hidden (`.`/`_`) names skipped, one store instance shared by all
    ///   parts; requires the `remote` feature (without it, the standard
    ///   `InputError::RemoteDisabled` as before);
    /// - `http(s)://` prefix → [`InputError::RemotePrefixUnsupported`]
    ///   (no generic listing API; the error points at `--files-from`);
    /// - a single resolved partition collapses to `Single`;
    /// - an empty directory/glob result → [`InputError::NoParquetInputs`].
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] for each failure listed above, and if a local
    /// path, glob, directory listing, or remote listing cannot be read, or if
    /// the resolved parts are incompatible.
    pub fn resolve(input: &str) -> Result<Self, InputError> {
        if let Some(_scheme) = url_scheme(input) {
            // A remote *prefix* ("directory", trailing `/`) is recognized
            // only when remote support is compiled in; without the feature
            // every remote URL fails with the standard `RemoteDisabled`
            // error exactly as before. Extension-less non-slash URLs
            // (presigned / API endpoints that serve parquet) are single
            // objects, matching pre-v0.7 behavior.
            #[cfg(feature = "remote")]
            if !_scheme.eq_ignore_ascii_case("file")
                && is_remote_scheme(_scheme)
                && remote_url_is_prefix(input)
            {
                if _scheme.eq_ignore_ascii_case("http") || _scheme.eq_ignore_ascii_case("https") {
                    return Err(InputError::RemotePrefixUnsupported {
                        url: input.to_string(),
                    });
                }
                return Self::from_remote_prefix(input);
            }
            // `file://` maps to a local path, remote objects stay single,
            // unsupported schemes get the standard error — all exactly as
            // before.
            return Ok(ConvertSource::single(InputSource::from_str_input(input)?));
        }
        let path = Path::new(input);
        if path.is_file() {
            // Existing local file: byte-identical behavior to today.
            return Ok(ConvertSource::single(InputSource::Local(
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
        Ok(ConvertSource::single(InputSource::Local(
            path.to_path_buf(),
        )))
    }

    /// `Single` for one file, `Multi` for several, a clear error for none.
    fn from_local_files(input: &str, files: Vec<PathBuf>) -> Result<Self, InputError> {
        Self::from_parts(input, files.into_iter().map(InputSource::Local).collect())
    }

    /// `Single` for one part, `Multi` for several,
    /// [`InputError::NoParquetInputs`] for none. `parts` must already be in
    /// read order (the row-order invariant).
    fn from_parts(input: &str, mut parts: Vec<InputSource>) -> Result<Self, InputError> {
        match parts.len() {
            0 => Err(InputError::NoParquetInputs {
                input: input.to_string(),
            }),
            1 => Ok(ConvertSource::single(parts.remove(0))),
            _ => Ok(ConvertSource::Multi(MultiSource::from_sources(
                input.to_string(),
                parts,
            )?)),
        }
    }

    /// Resolve an `s3://`/`gs://` prefix by listing the store: every
    /// visible `.parquet` object under the prefix, sorted by key, all
    /// sharing ONE store instance (one credential resolution) with sizes
    /// taken from the listing (no per-part HEADs).
    #[cfg(feature = "remote")]
    fn from_remote_prefix(input: &str) -> Result<Self, InputError> {
        let sources = crate::input::remote::sources_under_prefix(input)?;
        Self::from_parts(
            input,
            sources.into_iter().map(InputSource::Remote).collect(),
        )
    }

    /// Build a source from a `--files-from` manifest: one local path or
    /// remote URL per line, `#`-prefixed comment lines and blank lines
    /// skipped, entries trimmed. Line order is preserved VERBATIM — never
    /// sorted — because the converter's row-order invariant keys winner
    /// tables by global row offset; reordering the manifest reorders the
    /// dataset. Each line is resolved as a SINGLE file/object (no
    /// directory, glob, or prefix expansion); mixing local and remote
    /// entries is allowed (compatibility is validated as usual).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if the manifest cannot be read, lists no
    /// entries, or names a missing local file or an entry that cannot be
    /// opened, or if the entries are incompatible.
    pub fn from_manifest(manifest: &Path) -> Result<Self, InputError> {
        let text = std::fs::read_to_string(manifest).map_err(|e| InputError::ManifestRead {
            path: manifest.display().to_string(),
            source: e,
        })?;
        let entries: Vec<(String, String)> = manifest_entries(&text)
            .into_iter()
            .map(|(line, entry)| {
                (
                    format!("line {line} of manifest {}", manifest.display()),
                    entry.to_string(),
                )
            })
            .collect();
        Self::from_explicit_list(&manifest.display().to_string(), &entries)
    }

    /// Build a source from an explicit ordered list of inputs (local paths
    /// or URLs) — the Python `list[str]` input shape. Order is preserved
    /// verbatim; each entry is a single file/object (no expansion).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if `inputs` is empty, if an entry is a missing
    /// local file or cannot be opened, or if the entries are incompatible.
    pub fn from_input_list<S: AsRef<str>>(inputs: &[S]) -> Result<Self, InputError> {
        let entries: Vec<(String, String)> = inputs
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                (
                    format!("input list entry {}", i + 1),
                    entry.as_ref().to_string(),
                )
            })
            .collect();
        Self::from_explicit_list("input list", &entries)
    }

    /// Shared by [`Self::from_manifest`] and [`Self::from_input_list`]:
    /// resolve each `(context, entry)` as a single source, requiring local
    /// entries to exist up front so the error can name the entry instead
    /// of surfacing as a bare I/O error at footer-load time. Remote entries
    /// connect (one HEAD each) under bounded concurrency
    /// ([`LIST_CONNECT_CONCURRENCY`]); entry order is preserved verbatim
    /// in the resulting parts (the row-order invariant).
    fn from_explicit_list(root: &str, entries: &[(String, String)]) -> Result<Self, InputError> {
        let parts = resolve_list_entries(entries, |context, entry| {
            let src = InputSource::from_str_input(entry)?;
            match &src {
                InputSource::Local(p) if !p.is_file() => {
                    return Err(InputError::MissingListedInput {
                        context: context.to_string(),
                        input: entry.to_string(),
                    });
                }
                _ => {}
            }
            Ok(src)
        })?;
        Self::from_parts(root, parts)
    }

    /// [`ConvertSource::resolve`] for `Path` inputs (non-UTF-8 paths fall
    /// back to a single local source, as [`InputSource::from_path`] does).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] in the cases [`ConvertSource::resolve`] does.
    pub fn resolve_path(path: &Path) -> Result<Self, InputError> {
        match path.to_str() {
            Some(s) => Self::resolve(s),
            None => Ok(ConvertSource::single(InputSource::from_path(path)?)),
        }
    }

    /// The underlying parts, in read order (a single source is one part).
    pub fn parts(&self) -> &[InputSource] {
        match self {
            ConvertSource::Single(s) => std::slice::from_ref(s.input()),
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
            ConvertSource::Single(s) => s.input().display_name(),
            ConvertSource::Multi(m) => {
                format!("{} ({} partitions)", m.root, m.parts.len())
            }
        }
    }

    /// The Arrow schema of the dataset. For a multi source this is the
    /// validated union schema (nullability OR-ed across parts). After
    /// [`Self::restrict_columns`], only the kept columns, in file order.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read or the
    /// column restriction cannot be applied to the schema.
    pub fn schema(&self) -> Result<SchemaRef, InputError> {
        let full = match self {
            ConvertSource::Single(s) => s.meta()?.schema.clone(),
            ConvertSource::Multi(m) => m.schema.clone(),
        };
        Ok(match self.column_projection() {
            None => full,
            Some(keep) => Arc::new(full.project(keep)?),
        })
    }

    /// The unprojected schema: every column the files carry, whether or not
    /// [`Self::restrict_columns`] has narrowed what reads return.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub fn file_schema(&self) -> Result<SchemaRef, InputError> {
        match self {
            ConvertSource::Single(s) => Ok(s.meta()?.schema.clone()),
            ConvertSource::Multi(m) => Ok(m.schema.clone()),
        }
    }

    /// Restrict every later read — and [`Self::schema`] — to these root
    /// columns of the file schema, sorted ascending (#386). Applied once,
    /// before anything derives column indices from the schema, so every
    /// downstream index is already relative to the projected layout and the
    /// excluded columns are never decoded, and a remote input never stages
    /// their chunks (#400, [`Self::staging_plan`]). A second call is a
    /// programming error.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read, if `keep`
    /// is not a strictly ascending subset of the file's columns, or if the
    /// restriction was already set.
    pub fn restrict_columns(&self, keep: Vec<usize>) -> Result<(), InputError> {
        let ncols = self.file_schema()?.fields().len();
        if keep.windows(2).any(|w| w[0] >= w[1]) || keep.iter().any(|&i| i >= ncols) {
            return Err(InputError::Arrow(arrow_schema::ArrowError::SchemaError(
                format!("column restriction {keep:?} is not a sorted subset of 0..{ncols}"),
            )));
        }
        let cell = match self {
            ConvertSource::Single(s) => &s.projection,
            ConvertSource::Multi(m) => &m.projection,
        };
        cell.set(keep).map_err(|_| {
            InputError::Arrow(arrow_schema::ArrowError::SchemaError(
                "column restriction already applied to this source".to_string(),
            ))
        })
    }

    /// The root columns reads are restricted to, if any (see
    /// [`Self::restrict_columns`]).
    pub fn column_projection(&self) -> Option<&[usize]> {
        match self {
            ConvertSource::Single(s) => s.projection.get().map(Vec::as_slice),
            ConvertSource::Multi(m) => m.projection.get().map(Vec::as_slice),
        }
    }

    pub(crate) fn record_diagnostics(&self, sink: &crate::diagnostics::Diagnostics) {
        if let Self::Multi(source) = self {
            for meta in &source.metas {
                meta.resolved.record_diagnostics(sink);
            }
        }
    }

    /// Parquet key-value metadata of partition 0 (the `geo` metadata used
    /// for CRS detection; construction validated all parts agree).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub fn key_value_metadata(&self) -> Result<Option<Vec<KeyValue>>, InputError> {
        match self {
            ConvertSource::Single(s) => Ok(s
                .meta()?
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

    /// The cached schema interpretation of the first part. Construction proves
    /// all remaining parts agree on geometry and CRS.
    ///
    /// # Errors
    /// Returns an input error if a part's footer cannot be resolved.
    pub fn resolved_schema(&self) -> Result<&crate::resolved_schema::ResolvedSchema, InputError> {
        match self {
            ConvertSource::Single(s) => Ok(&s.meta()?.resolved),
            ConvertSource::Multi(m) => Ok(&m.metas[0].resolved),
        }
    }

    /// Cached descriptors for every physical part, in read order. Consumers
    /// using a common projection must not trust only the first part's footer.
    pub(crate) fn resolved_schemas(
        &self,
    ) -> Result<Vec<&crate::resolved_schema::ResolvedSchema>, InputError> {
        match self {
            Self::Single(s) => Ok(vec![s.meta()?.resolved.as_ref()]),
            Self::Multi(m) => Ok(m.metas.iter().map(|meta| meta.resolved.as_ref()).collect()),
        }
    }

    /// Total number of row groups across all parts.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub fn num_row_groups_total(&self) -> Result<usize, InputError> {
        Ok(self
            .metas()?
            .iter()
            .map(|m| m.parquet.num_row_groups())
            .sum())
    }

    /// Per-part `(row count, row-group count)` straight from each part's
    /// parquet footer, in read order.
    ///
    /// This is the content binding the convert plan's fingerprint pins
    /// (#511). Both facts come from the footer the source has already
    /// parsed, so they cost no extra I/O — and, unlike `fs::metadata`, they
    /// are available for a **remote** part as well as a local file. A remote
    /// object swapped under a plan changes its row count in every case that
    /// would otherwise corrupt the replay (the row-indexed winner table is
    /// addressed by row position).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub fn part_row_counts(&self) -> Result<Vec<(i64, usize)>, InputError> {
        Ok(self
            .metas()?
            .iter()
            .map(|m| {
                (
                    m.parquet.file_metadata().num_rows(),
                    m.parquet.num_row_groups(),
                )
            })
            .collect())
    }

    /// Total rows this source will actually STREAM under `selected`: the sum
    /// of `num_rows()` over the selected row groups of every part (every row
    /// group when `selected` is `None`), straight from the already-parsed
    /// footers.
    ///
    /// This is the domain of the convert plan's row-indexed winner tables
    /// (#511/#512). `part_row_counts` above only knows each part's TOTAL row
    /// count, which stops describing the replay as soon as `--bbox` or
    /// `--filter` prunes row groups — and that was exactly the hole a forged
    /// plan walked through. Row-group row counts are footer facts, so this
    /// costs no extra I/O and works for a remote part too.
    ///
    /// A selection index outside the part's row-group range contributes
    /// nothing rather than panicking: the caller compares the total and
    /// reports a mismatch.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub fn selected_row_count(
        &self,
        selected: Option<&RowGroupSelection>,
    ) -> Result<i64, InputError> {
        Ok(self
            .metas()?
            .iter()
            .enumerate()
            .map(|(i, m)| {
                crate::input::selected_groups(
                    &m.parquet,
                    selected.and_then(|s| s.parts().get(i)).map(Vec::as_slice),
                )
                .map(tylertoo_io::RowGroupMetaData::num_rows)
                .sum::<i64>()
            })
            .sum())
    }

    /// Bytes of one root column over the selected row groups (all of them
    /// when `selected` is `None`), straight from the footers: every leaf under
    /// the column, each counted as its uncompressed size or, for a byte-array
    /// leaf whose footer records it, its unencoded data bytes if larger (the
    /// #563 rule in [`tylertoo_io::read_batch_plan`]).
    ///
    /// `column` indexes the schema this source exposes, as a
    /// [`ReadPlan::projection`] does. Pass 1 reports this as the geometry
    /// column's weight (#305). It used to sum the Arrow memory of every batch
    /// it read, but that counts buffer capacity, and a zero-copy slice counts
    /// its parent's whole buffer, so the total depended on how the rows were
    /// read and spliced (#717). Footer bytes cannot.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub(crate) fn selected_column_bytes(
        &self,
        selected: Option<&RowGroupSelection>,
        column: usize,
    ) -> Result<u64, InputError> {
        let root = self.column_projection().map_or(column, |p| p[column]);
        let mut total = 0u64;
        for (i, m) in self.metas()?.iter().enumerate() {
            let descr = m.parquet.file_metadata().schema_descr();
            let leaves: Vec<usize> = (0..descr.num_columns())
                .filter(|&leaf| descr.get_column_root_idx(leaf) == root)
                .collect();
            for rg in crate::input::selected_groups(
                &m.parquet,
                selected.and_then(|s| s.parts().get(i)).map(Vec::as_slice),
            ) {
                for &leaf in &leaves {
                    let col = rg.column(leaf);
                    let bytes = col
                        .uncompressed_size()
                        .max(col.unencoded_byte_array_data_bytes().unwrap_or(0))
                        .max(0) as u64;
                    total = total.saturating_add(bytes);
                }
            }
        }
        Ok(total)
    }

    /// Per-part, per-row-group bboxes straight from the footers — the input
    /// the shard planner (#498) balances its cut on.
    ///
    /// Entry `[p][g]` is part `p`'s row group `g`, or `None` when neither
    /// statistics tier can supply a bbox. Footer-only, so a planet-scale
    /// input costs one footer fetch per part and no data page is read.
    pub(crate) fn part_row_group_bounds(
        &self,
    ) -> Result<Vec<Vec<Option<crate::covering::RowGroupBounds>>>, InputError> {
        Ok(self
            .metas()?
            .iter()
            .map(|m| m.resolved.bounds().to_vec())
            .collect())
    }

    /// Per-part, per-row-group row counts straight from the footers.
    ///
    /// Entry `[p][g]` is part `p`'s row group `g`'s `num_rows`. This is what
    /// turns a row-group *index* into a row *offset*, which is what a shard
    /// needs to re-address a global convert plan onto the subset of row groups
    /// it reads (#498): the plan's winner table is indexed by row position
    /// within the plan's own selected stream, so a shard has to know where
    /// each of its groups sits in that stream, not merely which groups it has.
    pub(crate) fn part_row_group_row_counts(&self) -> Result<Vec<Vec<i64>>, InputError> {
        Ok(self
            .metas()?
            .iter()
            .map(|m| {
                m.parquet
                    .row_groups()
                    .iter()
                    .map(tylertoo_io::RowGroupMetaData::num_rows)
                    .collect()
            })
            .collect())
    }

    /// A [`StreamRowLocator`] for the stream this source reads under
    /// `selected` (#553): footer row counts only, no data page read.
    pub(crate) fn stream_row_locator(
        &self,
        selected: Option<&RowGroupSelection>,
    ) -> Result<StreamRowLocator, InputError> {
        Ok(StreamRowLocator::new(
            &self.part_row_group_row_counts()?,
            selected,
        ))
    }

    /// Per-part bbox row-group selection (#102): applies the single-file
    /// covering-statistics pruning to each part independently.
    /// `bbox_units` is `[xmin, ymin, xmax, ymax]` in the file CRS units.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub fn select_row_groups(
        &self,
        bbox_units: &[f64; 4],
    ) -> Result<RowGroupSelection, InputError> {
        self.select_row_groups_any(std::slice::from_ref(bbox_units))
    }

    /// [`Self::select_row_groups`] keeping a row group that intersects any
    /// of `boxes`: a `--bbox` and its copies one world width away (#643).
    pub(crate) fn select_row_groups_any(
        &self,
        boxes: &[[f64; 4]],
    ) -> Result<RowGroupSelection, InputError> {
        let per_part = self
            .metas()?
            .iter()
            .map(|m| m.resolved.select_row_groups(boxes))
            .collect();
        Ok(RowGroupSelection(per_part))
    }

    /// Per-part attribute-filter row-group selection (#315): applies the
    /// bound filter's column-statistics pushdown
    /// ([`crate::overview::filter::BoundFilter::select_row_groups`]) to each
    /// part independently. Conservative — groups without usable statistics
    /// are kept.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
    pub fn select_row_groups_matching(
        &self,
        filter: &crate::overview::filter::BoundFilter,
    ) -> Result<RowGroupSelection, InputError> {
        let per_part = self
            .metas()?
            .iter()
            .map(|m| filter.select_row_groups(&m.parquet))
            .collect();
        Ok(RowGroupSelection(per_part))
    }

    /// Total *compressed* bytes of the selected row groups (`None` = every
    /// row group of every part) — the projected disk-spill size the #272
    /// free-space preflight consults. Per-part sums come from the shared
    /// single-file helper [`crate::input::selected_compressed_bytes`].
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read.
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
            // Saturating: each part's figure is footer-declared (#686).
            .fold(0, u64::saturating_add))
    }

    /// Plan reads for all selected parts through the untrusted-reader owner.
    pub(crate) fn read_batch_rows(
        &self,
        selected: Option<&RowGroupSelection>,
        requested: usize,
        limit: u64,
    ) -> Result<usize, InputError> {
        Ok(self
            .metas()?
            .iter()
            .enumerate()
            .fold(requested, |rows, (pi, m)| {
                tylertoo_io::read_batch_plan(
                    &m.parquet,
                    selected.and_then(|s| s.parts().get(pi)).map(Vec::as_slice),
                    rows,
                    limit,
                )
            }))
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

    /// The pass-0 staging plan (#788) for `selection` (`None` = every row
    /// group of every part): per remote part, range requests for the
    /// selected row groups' chunks of the columns this source reads
    /// ([`Self::column_projection`], every column when unset). Built from
    /// the footers alone. Pruned row groups and excluded columns are never
    /// part of a request, whatever the coalescing window.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read or
    /// declares a column chunk outside its object.
    pub(crate) fn staging_plan(
        &self,
        selection: Option<&RowGroupSelection>,
    ) -> Result<StagingPlan, InputError> {
        self.staging_plan_for(selection, self.column_projection())
    }

    /// [`Self::staging_plan`] for an explicit set of file-root columns
    /// (`None` = all), for a read narrower than the source's projection.
    pub(crate) fn staging_plan_for(
        &self,
        selection: Option<&RowGroupSelection>,
        roots: Option<&[usize]>,
    ) -> Result<StagingPlan, InputError> {
        let parts = self.parts();
        if let Some(sel) = selection {
            debug_assert_eq!(
                sel.0.len(),
                parts.len(),
                "row-group selection must cover every part"
            );
        }
        let parts = parts
            .iter()
            .enumerate()
            .map(|(i, part)| part.plan_staging(selection.map(|s| s.0[i].as_slice()), roots))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(StagingPlan { parts })
    }

    /// Bytes pass 0 stages to the local disk spill for `selection` (`None` =
    /// every row group of every part): the compressed size of every column
    /// chunk the conversion reads from a remote part, i.e. the selected row
    /// groups' chunks of the projected columns ([`Self::restrict_columns`]).
    /// Local parts contribute 0. This is the spill footprint the #272
    /// free-space preflight compares against, and (staging never fetches
    /// an excluded chunk or a pruned row group) the bytes staging fetches.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read or
    /// declares a column chunk outside its object.
    pub fn bytes_to_stage(&self, selection: Option<&RowGroupSelection>) -> Result<u64, InputError> {
        Ok(self.staging_plan(selection)?.bytes_to_stage())
    }

    /// Fetch `plan` into each remote part's disk spill (pass 0).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a fetch fails.
    pub(crate) fn stage(&self, plan: &StagingPlan) -> Result<(), InputError> {
        for (part, staging) in self.parts().iter().zip(&plan.parts) {
            part.stage_planned(staging)?;
        }
        Ok(())
    }

    /// Stage every part's selected row groups to local disk up front (pass 0,
    /// #286/#287) so the streaming passes read from disk, not the network:
    /// [`Self::staging_plan`] then [`Self::stage`]. `selection` (`None` = all
    /// row groups) is the per-part bbox pruning the passes will honor, so
    /// pruned groups are never fetched and total network traffic stays ≈1×
    /// the bytes read (#219); only the projected columns are fetched (#400).
    /// Errors are the caller's to handle; the streaming pipeline treats
    /// staging as best-effort (a network error here is one the passes would
    /// hit anyway).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if fetching a remote part's row groups or
    /// writing them to the spill fails.
    pub fn stage_selected(&self, selection: Option<&RowGroupSelection>) -> Result<(), InputError> {
        let plan = self.staging_plan(selection)?;
        self.stage(&plan)
    }

    /// Open a sequential batch stream over all parts (see [`SourceStream`]).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be read or the first
    /// part's reader cannot be opened.
    pub fn open_stream(&self, plan: &ReadPlan<'_>) -> Result<SourceStream<'_>, InputError> {
        let parts = self.parts();
        if let Some(sel) = plan.row_groups {
            debug_assert_eq!(
                sel.0.len(),
                parts.len(),
                "row-group selection must cover every part"
            );
        }
        // A plan's projection indexes the (possibly restricted) schema this
        // source exposes; the reader wants file-root indices. Compose the two.
        let projection = match (self.column_projection(), plan.projection) {
            (None, None) => None,
            (None, Some(cols)) => Some(cols.to_vec()),
            (Some(base), None) => Some(base.to_vec()),
            (Some(base), Some(cols)) => Some(cols.iter().map(|&c| base[c]).collect()),
        };
        // Footers are parsed once per source (lazily for `Single`, at
        // construction for `Multi`); every reader this stream opens reuses
        // them rather than re-decoding O(row_groups × columns) of thrift.
        let metas = self
            .metas()?
            .iter()
            .map(|m| m.reader.clone())
            .collect::<Vec<_>>();
        // #563: each part's reader caps the batch for its selected row
        // groups' widest byte-array column (`untrusted::ParquetSource::build`),
        // so no batch can pass arrow's i32 offset ceiling. Output does not
        // depend on the batch size; only the opaque overflow error does.
        Ok(SourceStream {
            parts,
            metas,
            projection,
            row_groups: plan.row_groups.map(|s| s.0.clone()),
            batch_size: plan.batch_size.max(1),
            part_idx: 0,
            current: None,
            done: false,
        })
    }

    /// Split this source's selected row groups into ordered reader segments of
    /// **at most** `target_rows` rows each (#494).
    ///
    /// Segments are emitted in exactly the order [`SourceStream`] would read
    /// them — parts in order, row groups ascending within a part — and never
    /// straddle a part boundary, which is what lets a set of parallel readers
    /// be merged back into a single in-order stream. A part with an empty
    /// selection yields no segments at all, matching the sequential reader,
    /// which never opens it.
    ///
    /// **That ordering promise is load-bearing twice over.** The parallel
    /// merge re-derives each batch's `row_offset` by counting rows over the
    /// merged stream from zero, so it is only the plan's addressing if the
    /// merged order is the sequential order. And a sharded build (#498)
    /// depends on the same property from the other side: the segments are cut
    /// from the row groups THIS run selected (bbox ∩ filter ∩ shard range), so
    /// a shard's `row_offset` counts its own narrower stream — which is
    /// exactly the stream
    /// [`overview::plan_state::rebase_plan_for_shard`](crate::overview::plan_state)
    /// re-addresses the convert plan's row-indexed tables onto, in the same
    /// part-then-ascending-row-group order. If either side's ordering ever
    /// changed, every shard would read the wrong winner byte — silently.
    ///
    /// **`target_rows` is a ceiling, not an average.** The caller sets it to
    /// what one reader can buffer ahead of the merge, and a segment that does
    /// not fit there makes its worker park mid-segment — which serializes the
    /// reads again, the exact thing the split exists to avoid. So a row group
    /// that would take the run past the target starts a new run instead of
    /// joining it, even though the run is then well under target (51 200-row
    /// row groups against a 65 536-row target give one row group per segment,
    /// not two overshooting to 102 400). The one unavoidable exception is a
    /// single row group larger than the target: row groups are never split, so
    /// that segment overshoots and the caller's warning fires.
    pub(crate) fn read_segments(
        &self,
        selection: Option<&RowGroupSelection>,
        target_rows: usize,
    ) -> Result<Vec<ReadSegment>, InputError> {
        let target_rows = target_rows.max(1);
        let metas = self.metas()?;
        let mut out = Vec::new();
        for (part, meta) in metas.iter().enumerate() {
            let selected: Vec<usize> = match selection {
                Some(sel) => sel.0[part].clone(),
                None => (0..meta.parquet.num_row_groups()).collect(),
            };
            let (mut run, mut run_rows) = (Vec::new(), 0usize);
            for rg in selected {
                let rows = meta.parquet.row_group(rg).num_rows().max(0) as usize;
                if !run.is_empty() && run_rows + rows > target_rows {
                    out.push(ReadSegment {
                        part,
                        row_groups: std::mem::take(&mut run),
                        rows: run_rows,
                    });
                    run_rows = 0;
                }
                run.push(rg);
                run_rows += rows;
            }
            if !run.is_empty() {
                out.push(ReadSegment {
                    part,
                    row_groups: run,
                    rows: run_rows,
                });
            }
        }
        Ok(out)
    }

    /// Open a batch stream over exactly one [`ReadSegment`].
    ///
    /// Expressed as an ordinary [`ReadPlan`] whose row-group selection is
    /// empty for every part but the segment's, so the reader construction,
    /// projection composition and part skipping are the single-stream ones —
    /// there is no second way to open an input. `projection` is the caller's
    /// [`ReadPlan::projection`], and `open_stream` composes it with the
    /// source's `column_projection` (#386) exactly as it does for the
    /// sequential read this must reproduce: `None` for pass 2's every column,
    /// pass 1's geometry and flag columns otherwise (#717).
    ///
    /// Callers running several of these CONCURRENTLY over the same source must
    /// be local-only: `SourceStream` releases a part's in-memory read cache
    /// when it finishes that part, which is a no-op for a local file but would
    /// have concurrent remote readers evicting each other's fetched chunks.
    pub(crate) fn open_segment(
        &self,
        segment: &ReadSegment,
        projection: Option<&[usize]>,
        batch_size: usize,
    ) -> Result<SourceStream<'_>, InputError> {
        let mut per_part = vec![Vec::new(); self.parts().len()];
        per_part[segment.part].clone_from(&segment.row_groups);
        let selection = RowGroupSelection::from_parts(per_part);
        self.open_stream(&ReadPlan {
            batch_size,
            projection,
            row_groups: Some(&selection),
        })
    }

    /// Footer metadata per part: borrowed for `Multi` (loaded at
    /// construction), loaded on demand for `Single`.
    fn metas(&self) -> Result<std::borrow::Cow<'_, [PartMeta]>, InputError> {
        match self {
            ConvertSource::Single(s) => {
                Ok(std::borrow::Cow::Borrowed(std::slice::from_ref(s.meta()?)))
            }
            ConvertSource::Multi(m) => Ok(std::borrow::Cow::Borrowed(&m.metas)),
        }
    }
}

/// Load one part's footer: Arrow schema + parsed parquet metadata. Cheap
/// for local files (OS page cache); remote sources reuse their cached
/// footer after the first open.
fn load_part_meta(source: &InputSource) -> Result<PartMeta, InputError> {
    let builder = source.open()?;
    let parquet = builder.metadata().clone();
    // The footer and schema the owner already checked and decoded (#779):
    // every later reader over this part is built from it, with no second
    // footer parse and no second `ARROW:schema` decode.
    let reader = builder.reader_metadata().clone();
    let resolved = crate::resolved_schema::ResolvedSchema::from_parquet(builder.schema(), &parquet)
        .map_err(|e| InputError::Parquet(tylertoo_io::ParquetError::General(e.to_string())))?;
    Ok(PartMeta {
        resolved: Arc::new(resolved),
        schema: builder.schema().clone(),
        parquet,
        reader,
    })
}

/// Ceiling on concurrent footer loads in [`load_part_metas`]. A remote
/// footer costs two range requests; loading hundreds of parts serially
/// pays hundreds of sequential round-trips, while unbounded parallelism
/// would open one connection per part. Eight in flight keeps a
/// hundreds-of-parts prefix listing responsive without a connection storm.
const FOOTER_LOAD_CONCURRENCY: usize = 8;

/// Load every part's footer, in order, with bounded concurrency. Results
/// are written into per-index slots so the returned order always matches
/// `parts` regardless of completion order; the first error (by part index)
/// wins.
#[expect(
    clippy::disallowed_types,
    reason = "#779: per-operation caches, worker coordination and profiling; no diagnostic globals"
)]
fn load_part_metas(parts: &[InputSource]) -> Result<Vec<PartMeta>, InputError> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let workers = FOOTER_LOAD_CONCURRENCY.min(parts.len());
    if workers <= 1 {
        return parts.iter().map(load_part_meta).collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Result<PartMeta, InputError>>>> = (0..parts.len())
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= parts.len() {
                    break;
                }
                let meta = load_part_meta(&parts[i]);
                *slots[i].lock().expect("footer slot lock") = Some(meta);
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .expect("footer slot lock")
                .expect("every slot filled by a worker")
        })
        .collect()
}

impl MultiSource {
    /// Build a multi source over already-constructed parts: load every
    /// part's footer and validate compatibility against part 0 (see the
    /// module docs). `parts` must be non-empty and already ordered.
    /// Footer loads run with bounded concurrency
    /// (`FOOTER_LOAD_CONCURRENCY`): a remote footer is two range
    /// requests, so hundreds of parts would otherwise serialize hundreds
    /// of round-trips — but must not open hundreds of connections at once
    /// either.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if a part's footer cannot be loaded, or if a
    /// part's schema, geometry column, or CRS is incompatible with part 0.
    ///
    /// # Panics
    ///
    /// Panics if `parts` is empty.
    #[expect(
        clippy::disallowed_types,
        reason = "#779: initializes per-input or per-tile worker caches"
    )]
    pub fn from_sources(root: String, parts: Vec<InputSource>) -> Result<Self, InputError> {
        assert!(!parts.is_empty(), "MultiSource requires at least one part");
        let metas = load_part_metas(&parts)?;

        let first_name = parts[0].display_name();
        // The reference CRS: `Ok(crs)` per detect_crs_from_kv, `None` for an
        // unsupported/undetectable CRS. Parts must AGREE; supportedness
        // itself is enforced by the pipeline (against part 0), so a set
        // that consistently carries one unsupported CRS still errors with
        // the standard UnsupportedCrs message.
        // Only Ok/Err is used here, so the error's input name is moot.
        let crs_of = |m: &PartMeta| m.resolved.crs();
        let raw_crs_of = |m: &PartMeta| -> String {
            let info = m.resolved.crs_info();
            info.identifier
                .clone()
                .or_else(|| info.name.clone())
                .unwrap_or_else(|| "unknown".to_string())
        };
        let first_crs = crs_of(&metas[0]);

        for (part, meta) in parts.iter().zip(&metas).skip(1) {
            let incompatible = |detail: String| InputError::IncompatiblePartition {
                first: first_name.clone(),
                offender: part.display_name(),
                detail,
            };
            validate_schema_shape(&metas[0].schema, &meta.schema).map_err(incompatible)?;
            if meta.resolved.geometry_name() != metas[0].resolved.geometry_name() {
                return Err(incompatible(format!(
                    "primary geometry mismatch: {:?} versus {:?}",
                    meta.resolved.geometry_name(),
                    metas[0].resolved.geometry_name()
                )));
            }
            let crs = crs_of(meta);
            if crs != first_crs {
                return Err(incompatible(format!(
                    "CRS mismatch: partition declares {:?} but the first partition \
                     declares {:?} (all partitions must share one CRS)",
                    raw_crs_of(meta),
                    raw_crs_of(&metas[0]),
                )));
            }
            if crs.is_none() {
                // Both undetectable: still require the RAW declarations to
                // agree, so the eventual UnsupportedCrs error is truthful.
                let (a, b) = (raw_crs_of(&metas[0]), raw_crs_of(meta));
                if a != b {
                    return Err(incompatible(format!(
                        "CRS mismatch: partition declares {b:?} but the first \
                         partition declares {a:?} (all partitions must share \
                         one CRS)"
                    )));
                }
            }
        }

        let schema = union_schema(&metas);
        Ok(MultiSource {
            root,
            parts,
            metas,
            schema,
            projection: std::sync::OnceLock::new(),
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
                 the first partition ({}) — geometry encoding/CRS metadata must \
                 match",
                fi.name(),
                describe_metadata_diff(f0.metadata(), fi.metadata())
            ));
        }
    }
    Ok(())
}

/// Human-readable first difference between two field-metadata maps:
/// the mismatching key plus a truncated value diff (or which side is
/// missing the key). Byte-exact comparison can false-reject cross-writer
/// partitions (e.g. semantically equal PROJJSON serialized differently),
/// so the error must show the user exactly what to reconcile.
fn describe_metadata_diff(
    first: &std::collections::HashMap<String, String>,
    other: &std::collections::HashMap<String, String>,
) -> String {
    fn trunc(s: &str) -> String {
        const MAX: usize = 80;
        if s.chars().count() > MAX {
            let cut: String = s.chars().take(MAX).collect();
            format!("{cut:?}…")
        } else {
            format!("{s:?}")
        }
    }
    let mut keys: Vec<&String> = first.keys().chain(other.keys()).collect();
    keys.sort();
    keys.dedup();
    for key in keys {
        match (first.get(key), other.get(key)) {
            (Some(a), Some(b)) if a != b => {
                return format!(
                    "key {key:?}: first partition has {} but this partition has {}",
                    trunc(a),
                    trunc(b)
                );
            }
            (Some(_), None) => {
                return format!("key {key:?} is present only in the first partition");
            }
            (None, Some(_)) => {
                return format!("key {key:?} is present only in this partition");
            }
            _ => {}
        }
    }
    "maps differ".to_string()
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
    fn open_part(&self, i: usize) -> Result<tylertoo_io::UntrustedBatchReader, InputError> {
        let mut builder = self.parts[i].open_with_metadata(&self.metas[i])?;
        if let Some(cols) = &self.projection {
            builder = builder.with_projection_roots(cols.iter().copied());
        }
        if let Some(sel) = &self.row_groups {
            builder = builder.with_row_groups(sel[i].clone());
        }
        Ok(builder.with_batch_rows(self.batch_size).build()?)
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

/// Entry-resolution concurrency for explicit lists (`--files-from`
/// manifests, Python input lists). Remote entries cost one connect (HEAD)
/// each, so hundreds of manifest lines would otherwise serialize hundreds
/// of round-trips; matches [`FOOTER_LOAD_CONCURRENCY`].
const LIST_CONNECT_CONCURRENCY: usize = 8;

/// Resolve every `(context, entry)` pair with `resolve` under bounded
/// concurrency ([`LIST_CONNECT_CONCURRENCY`]). Results are written into
/// per-index slots, so the returned vector always matches `entries` order
/// regardless of completion order — the row-order invariant — and the
/// first error (by entry index) wins.
#[expect(
    clippy::disallowed_types,
    reason = "#779: per-input metadata caches and per-call worker coordination"
)]
fn resolve_list_entries<F>(
    entries: &[(String, String)],
    resolve: F,
) -> Result<Vec<InputSource>, InputError>
where
    F: Fn(&str, &str) -> Result<InputSource, InputError> + Sync,
{
    use std::sync::atomic::{AtomicUsize, Ordering};
    let workers = LIST_CONNECT_CONCURRENCY.min(entries.len());
    if workers <= 1 {
        return entries
            .iter()
            .map(|(context, entry)| resolve(context, entry))
            .collect();
    }
    let next = AtomicUsize::new(0);
    let slots: Vec<std::sync::Mutex<Option<Result<InputSource, InputError>>>> = (0..entries.len())
        .map(|_| std::sync::Mutex::new(None))
        .collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= entries.len() {
                    break;
                }
                let (context, entry) = &entries[i];
                *slots[i].lock().expect("entry slot lock") = Some(resolve(context, entry));
            });
        }
    });
    slots
        .into_iter()
        .map(|slot| {
            slot.into_inner()
                .expect("entry slot lock")
                .expect("every slot filled by a worker")
        })
        .collect()
}

/// Parse `--files-from` manifest text into `(1-based line number, entry)`
/// pairs: entries are trimmed; blank lines and lines whose first non-space
/// character is `#` are skipped. Order is preserved verbatim (see
/// [`ConvertSource::from_manifest`]).
pub(crate) fn manifest_entries(text: &str) -> Vec<(usize, &str)> {
    text.lines()
        .enumerate()
        .filter_map(|(i, line)| {
            let entry = line.trim();
            (!entry.is_empty() && !entry.starts_with('#')).then_some((i + 1, entry))
        })
        .collect()
}

/// Whether a remote URL names a *prefix* ("directory"): its path component
/// — query string and fragment stripped — ends with `/`. Everything else,
/// including extension-less presigned/API URLs that serve parquet, is a
/// single object (the pre-v0.7 classification).
// Used by `resolve` only when remote support is compiled in; the pure
// classification is unit-tested under every feature set.
fn remote_url_is_prefix(url: &str) -> bool {
    let no_fragment = url.split('#').next().unwrap_or(url);
    let no_query = no_fragment.split('?').next().unwrap_or(no_fragment);
    no_query.ends_with('/')
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
        // #452: name the directory; a bare `InputError::Io` did not.
        let open = |error| InputError::Open {
            input: dir.display().to_string(),
            error,
        };
        for entry in std::fs::read_dir(dir).map_err(open)? {
            let path = entry.map_err(open)?.path();
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
        // glob 0.3.3 dropped `impl From<GlobError> for io::Error` and 0.3.4
        // restored it, deprecating `into_error`. The floor in Cargo.toml is
        // 0.3.4 so that `into` is the one form that compiles warning-free.
        // #452: and name the path the glob could not read.
        let path = entry.map_err(|e| InputError::Open {
            input: e.path().display().to_string(),
            error: e.into(),
        })?;
        if path.is_file() && path.extension().is_some_and(|ext| ext == "parquet") {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
}

/// Default PMTiles layer name derived from a CLI-style input string — the
/// multi-partition generalization of "the input file's stem":
///
/// - single file (local path or nonexistent-yet path) → file stem
///   (historical behavior; also what a `--files-from` manifest path gives:
///   the manifest file's stem);
/// - existing local directory → the directory's last path segment,
///   verbatim (a dotted directory name is a name, not an extension);
/// - glob pattern → the deepest literal (wildcard-free) path segment
///   before the first wildcard segment;
/// - remote URL (`scheme://…`, query string and fragment stripped):
///   trailing-slash prefix → last non-empty path segment verbatim
///   (bucket name for a bucket-root prefix); single object → the key's
///   last segment's stem;
/// - anything degenerate (empty, no usable segment) → `"layer"`, the
///   same fallback the single-file path has always used.
pub fn derive_layer_name(input: &str) -> String {
    const FALLBACK: &str = "layer";
    let stem_of = |p: &Path| p.file_stem().and_then(|s| s.to_str()).map(str::to_string);

    let name = if url_scheme(input).is_some() {
        let no_fragment = input.split('#').next().unwrap_or(input);
        let no_query = no_fragment.split('?').next().unwrap_or(no_fragment);
        let rest = no_query
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(no_query);
        rest.split('/')
            .rev()
            .find(|seg| !seg.is_empty())
            .and_then(|seg| {
                if remote_url_is_prefix(input) {
                    Some(seg.to_string())
                } else {
                    stem_of(Path::new(seg))
                }
            })
    } else {
        let path = Path::new(input);
        if path.is_dir() {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        } else if !path.is_file() && has_glob_meta(input) {
            // Deepest literal segment before the first wildcard one.
            let mut last_literal = None;
            for comp in path.components() {
                if let std::path::Component::Normal(seg) = comp {
                    match seg.to_str() {
                        Some(s) if !has_glob_meta(s) => last_literal = Some(s.to_string()),
                        // Wildcard (or non-UTF-8) segment: stop descending.
                        _ => break,
                    }
                }
            }
            last_literal
        } else {
            stem_of(path)
        }
    };
    name.filter(|n| !n.is_empty())
        .unwrap_or_else(|| FALLBACK.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{ArrayRef, Int64Array, StringArray};
    use arrow_schema::DataType;
    use std::fs::File;
    use tylertoo_io::test_support::ArrowWriter;
    use tylertoo_io::test_support::WriterProperties;

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
    #[cfg(test)]
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

    /// #452: an unreadable partition directory names itself.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_directory_is_named() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root = std::fs::read_dir(&locked).is_ok();
        let listed = list_parquet_files(dir.path());
        let pattern = format!("{}/*/*.parquet", dir.path().display());
        let globbed = expand_glob(&pattern);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        // root reads any directory: nothing to assert there.
        if root {
            return;
        }
        for (what, result) in [("directory", listed.map(drop)), ("glob", globbed.map(drop))] {
            let msg = result.expect_err(what).to_string();
            assert!(msg.contains(&locked.display().to_string()), "{what}: {msg}");
        }
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

    /// Pure prefix classification: ONLY a trailing-slash path is a prefix.
    /// Extension-less non-slash URLs (presigned / API download endpoints
    /// that serve parquet) must stay single objects, as on main.
    #[test]
    fn remote_prefix_is_trailing_slash_only() {
        assert!(remote_url_is_prefix("s3://bucket/dataset/"));
        assert!(remote_url_is_prefix("gs://bucket/prefix/"));
        assert!(remote_url_is_prefix("https://example.com/data/?list=1"));
        assert!(remote_url_is_prefix("https://example.com/data/#frag"));
        assert!(!remote_url_is_prefix("s3://bucket/key.parquet"));
        assert!(!remote_url_is_prefix(
            "https://h/k.parquet?X-Amz-Signature=abc"
        ));
        assert!(!remote_url_is_prefix(
            "https://host/api/datasets/42/download"
        ));
        assert!(!remote_url_is_prefix("s3://bucket/dataset"));
    }

    /// http(s) prefixes stay hard errors (generic HTTP has no listing API)
    /// and the error points at `--files-from`. s3/gs prefixes are listed
    /// for real now (covered by the `InMemory` tests below), so they are NOT
    /// rejected here.
    #[cfg(feature = "remote")]
    #[test]
    fn resolve_https_prefix_points_at_files_from() {
        for url in ["https://example.com/data/", "http://example.com/data/"] {
            let err = ConvertSource::resolve(url).unwrap_err();
            assert!(
                matches!(err, InputError::RemotePrefixUnsupported { .. }),
                "{url} → {err:?}"
            );
            let msg = err.to_string();
            assert!(
                msg.contains("--files-from"),
                "error must point at --files-from: {msg}"
            );
        }
    }

    /// Without the `remote` feature every remote URL — prefix-shaped or
    /// not — keeps main's behavior: `RemoteDisabled`, never the misleading
    /// "prefix listing coming later" message. An extension-less URL
    /// reaching `RemoteDisabled` also proves it was classified as a single
    /// object (the prefix arm would have returned earlier).
    #[cfg(not(feature = "remote"))]
    #[test]
    fn remote_disabled_behavior_unchanged() {
        for url in [
            "s3://bucket/prefix/",
            "s3://bucket/key.parquet",
            "https://host/api/datasets/42/download",
        ] {
            let err = ConvertSource::resolve(url).unwrap_err();
            assert!(
                matches!(err, InputError::RemoteDisabled(_)),
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

    /// Two partitions with *different unsupported* CRSs must be rejected at
    /// set-construction time (naming both raw values), not "agree on
    /// undetectable" and error later with only part 0's CRS.
    #[test]
    fn different_unsupported_crs_rejected_with_both_values() {
        let dir = tmpdir();
        let geo = |epsg: u32| {
            format!(
                r#"{{"version":"1.0.0","primary_column":"geometry","columns":{{"geometry":{{"crs":"EPSG:{epsg}"}}}}}}"#
            )
        };
        for (file, epsg) in [("a.parquet", 32633u32), ("b.parquet", 32634u32)] {
            let path = dir.path().join(file);
            let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)]));
            let batch = RecordBatch::try_new(
                schema.clone(),
                vec![Arc::new(Int64Array::from(vec![1i64])) as ArrayRef],
            )
            .unwrap();
            let file = File::create(&path).unwrap();
            let mut writer = ArrowWriter::try_new(file, schema, None).unwrap();
            writer.write(&batch).unwrap();
            writer.append_key_value_metadata(tylertoo_io::KeyValue::new(
                "geo".to_string(),
                geo(epsg),
            ));
            writer.close().unwrap();
        }
        let err = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap_err();
        match err {
            InputError::IncompatiblePartition {
                offender, detail, ..
            } => {
                assert!(offender.ends_with("b.parquet"), "offender: {offender}");
                assert!(
                    detail.contains("32633") && detail.contains("32634"),
                    "detail must name both raw CRS values: {detail}"
                );
            }
            other => panic!("expected IncompatiblePartition, got {other:?}"),
        }
    }

    /// A field-metadata mismatch must say WHAT differs: the key and a
    /// (truncated) value diff, so cross-writer CRS/encoding differences are
    /// actionable instead of a bare "metadata differs".
    #[test]
    fn metadata_mismatch_detail_names_key_and_values() {
        let dir = tmpdir();
        for (file, val) in [("a.parquet", "value-one"), ("b.parquet", "value-two")] {
            let md: std::collections::HashMap<String, String> =
                [("ARROW:extension:name".to_string(), val.to_string())].into();
            write_parquet(
                &dir.path().join(file),
                vec![Field::new("id", DataType::Int64, false).with_metadata(md)],
                vec![Arc::new(Int64Array::from(vec![1i64]))],
                None,
            );
        }
        let err = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap_err();
        match err {
            InputError::IncompatiblePartition { detail, .. } => {
                assert!(
                    detail.contains("ARROW:extension:name"),
                    "detail must name the differing key: {detail}"
                );
                assert!(
                    detail.contains("value-one") && detail.contains("value-two"),
                    "detail must show both values: {detail}"
                );
            }
            other => panic!("expected IncompatiblePartition, got {other:?}"),
        }
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

    // --- reader segments (#494) ------------------------------------------

    /// Write `rows` rows into `path` with `rg_rows` rows per row group.
    fn write_rowgroups(path: &Path, rows: i64, rg_rows: usize) {
        write_parquet(
            path,
            vec![Field::new("id", DataType::Int64, false)],
            vec![Arc::new(Int64Array::from((0..rows).collect::<Vec<_>>())) as ArrayRef],
            Some(rg_rows),
        );
    }

    /// `target_rows` is a CEILING. A run that would overshoot it starts a new
    /// segment instead, because the caller sizes the target to what a reader
    /// can buffer ahead of the merge — an overshooting segment parks its
    /// worker mid-run and serializes the reads again.
    #[test]
    fn read_segments_never_overshoot_the_target() {
        let dir = tmpdir();
        let f = dir.path().join("a.parquet");
        // 8 row groups of 100 rows, target 150: two row groups would be 200.
        write_rowgroups(&f, 800, 100);
        let src = ConvertSource::resolve(f.to_str().unwrap()).unwrap();
        let segments = src.read_segments(None, 150).unwrap();
        assert_eq!(segments.len(), 8, "one row group per segment: {segments:?}");
        assert!(segments.iter().all(|s| s.rows == 100 && s.part == 0));
        // A target that fits two row groups takes two.
        let segments = src.read_segments(None, 200).unwrap();
        assert_eq!(segments.len(), 4);
        assert!(segments.iter().all(|s| s.rows == 200));
        // Row groups stay in ascending order and cover the selection exactly.
        let all: Vec<usize> = segments
            .iter()
            .flat_map(|s| s.row_groups.iter().copied())
            .collect();
        assert_eq!(all, (0..8).collect::<Vec<_>>());
    }

    /// A row group larger than the target is the one case a segment may
    /// overshoot: row groups are never split.
    #[test]
    fn read_segments_keep_an_oversize_row_group_whole() {
        let dir = tmpdir();
        let f = dir.path().join("a.parquet");
        write_rowgroups(&f, 600, 300);
        let src = ConvertSource::resolve(f.to_str().unwrap()).unwrap();
        let segments = src.read_segments(None, 50).unwrap();
        assert_eq!(segments.len(), 2);
        assert!(segments.iter().all(|s| s.rows == 300));
    }

    /// Segments never straddle a part, and a part whose selection is empty
    /// contributes none — the sequential reader never opens it either.
    #[test]
    fn read_segments_respect_parts_and_selection() {
        let dir = tmpdir();
        write_rowgroups(&dir.path().join("p0.parquet"), 400, 100);
        write_rowgroups(&dir.path().join("p1.parquet"), 200, 100);
        write_rowgroups(&dir.path().join("p2.parquet"), 300, 100);
        let src = ConvertSource::resolve(dir.path().to_str().unwrap()).unwrap();

        let segments = src.read_segments(None, 1000).unwrap();
        assert_eq!(
            segments.iter().map(|s| s.part).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "one segment per part when the target swallows a whole part"
        );

        // Part 1 pruned away entirely.
        let selection = RowGroupSelection::from_parts(vec![vec![0, 1], Vec::new(), vec![2]]);
        let segments = src.read_segments(Some(&selection), 1000).unwrap();
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].part, 0);
        assert_eq!(segments[0].row_groups, vec![0, 1]);
        assert_eq!(segments[1].part, 2);
        assert_eq!(segments[1].row_groups, vec![2]);
    }

    /// #717: a root column's bytes come from the footer, summed over every
    /// leaf under it (a nested column has several) and over the selected row
    /// groups only, so they cannot depend on how the rows are read.
    #[test]
    fn selected_column_bytes_sums_leaves_over_the_selection() {
        use arrow_array::{Float64Array, StructArray};

        let dir = tmpdir();
        let path = dir.path().join("p0.parquet");
        let n = 400usize;
        let xs = Float64Array::from((0..n).map(|i| i as f64).collect::<Vec<_>>());
        let ys = Float64Array::from((0..n).map(|i| -(i as f64)).collect::<Vec<_>>());
        let pt_fields = vec![
            Field::new("x", DataType::Float64, false),
            Field::new("y", DataType::Float64, false),
        ];
        let pt = StructArray::new(
            pt_fields.clone().into(),
            vec![Arc::new(xs) as ArrayRef, Arc::new(ys) as ArrayRef],
            None,
        );
        write_parquet(
            &path,
            vec![
                Field::new("id", DataType::Int64, false),
                Field::new("pt", DataType::Struct(pt_fields.into()), false),
                Field::new("name", DataType::Utf8, true),
            ],
            vec![
                Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
                Arc::new(pt),
                Arc::new(StringArray::from(
                    (0..n).map(|i| format!("r{i}")).collect::<Vec<_>>(),
                )),
            ],
            Some(100),
        );
        let src = ConvertSource::resolve(path.to_str().unwrap()).unwrap();

        // Leaves 1 and 2 (pt.x, pt.y) sit under root column 1.
        let meta = tylertoo_io::read_parquet_metadata(&File::open(&path).unwrap()).unwrap();
        assert_eq!(meta.num_row_groups(), 4);
        let rg_bytes = |g: usize| -> u64 {
            let rg = meta.row_group(g);
            (rg.column(1).uncompressed_size() + rg.column(2).uncompressed_size()) as u64
        };

        assert_eq!(
            src.selected_column_bytes(None, 1).unwrap(),
            (0..4).map(rg_bytes).sum::<u64>()
        );
        let selection = RowGroupSelection::from_parts(vec![vec![1, 3]]);
        assert_eq!(
            src.selected_column_bytes(Some(&selection), 1).unwrap(),
            rg_bytes(1) + rg_bytes(3)
        );
        let none = RowGroupSelection::from_parts(vec![Vec::new()]);
        assert_eq!(src.selected_column_bytes(Some(&none), 1).unwrap(), 0);
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

    // --- column restriction (#386) -------------------------------------------

    /// Three-column fixture: `id`, `name`, `extra`.
    #[cfg(test)]
    fn write_three(path: &Path, ids: Vec<i64>) {
        let n = ids.len();
        write_parquet(
            path,
            vec![
                Field::new("id", DataType::Int64, false),
                Field::new("name", DataType::Utf8, true),
                Field::new("extra", DataType::Int64, true),
            ],
            vec![
                Arc::new(Int64Array::from(ids)),
                Arc::new(StringArray::from(
                    (0..n).map(|i| format!("r{i}")).collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
            ],
            None,
        );
    }

    #[test]
    fn restrict_columns_narrows_schema_and_every_read() {
        for multi in [false, true] {
            let dir = tmpdir();
            write_three(&dir.path().join("p0.parquet"), vec![0, 1]);
            if multi {
                write_three(&dir.path().join("p1.parquet"), vec![2]);
            }
            let input = if multi {
                dir.path().to_path_buf()
            } else {
                dir.path().join("p0.parquet")
            };
            let src = ConvertSource::resolve(input.to_str().unwrap()).unwrap();
            assert_eq!(src.schema().unwrap().fields().len(), 3);

            // Keep id + extra (drop the middle column, so indices shift).
            src.restrict_columns(vec![0, 2]).unwrap();
            let names: Vec<String> = src
                .schema()
                .unwrap()
                .fields()
                .iter()
                .map(|f| f.name().clone())
                .collect();
            assert_eq!(names, vec!["id", "extra"], "multi={multi}");
            assert_eq!(src.file_schema().unwrap().fields().len(), 3);

            // A full read returns only the kept columns.
            let plan = ReadPlan {
                batch_size: 8,
                projection: None,
                row_groups: None,
            };
            let mut rows = 0;
            for batch in src.open_stream(&plan).unwrap() {
                let batch = batch.unwrap();
                assert_eq!(batch.num_columns(), 2);
                assert_eq!(batch.schema().field(1).name(), "extra");
                rows += batch.num_rows();
            }
            assert_eq!(rows, if multi { 3 } else { 2 });

            // A plan projection indexes the restricted schema: 1 = extra.
            let cols = [1usize];
            let plan = ReadPlan {
                batch_size: 8,
                projection: Some(&cols),
                row_groups: None,
            };
            for batch in src.open_stream(&plan).unwrap() {
                let batch = batch.unwrap();
                assert_eq!(batch.num_columns(), 1);
                assert_eq!(batch.schema().field(0).name(), "extra");
            }

            // Applying twice, or an unsorted / out-of-range set, is refused.
            assert!(src.restrict_columns(vec![0]).is_err());
            let fresh = ConvertSource::resolve(input.to_str().unwrap()).unwrap();
            assert!(fresh.restrict_columns(vec![2, 0]).is_err());
            assert!(fresh.restrict_columns(vec![0, 7]).is_err());
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

    // --- --files-from manifest (v0.7 PR-B) ------------------------------------

    /// Comments, blank lines, and surrounding whitespace are skipped;
    /// entry order is preserved VERBATIM (never sorted) — the row-order
    /// invariant keys winner tables by global row offset.
    #[test]
    fn manifest_entries_skip_comments_and_preserve_order() {
        let text = "\
# heading comment
b.parquet

  # indented comment
  a.parquet  \n\nz/c.parquet\n";
        let entries = manifest_entries(text);
        assert_eq!(
            entries,
            vec![(2, "b.parquet"), (5, "a.parquet"), (7, "z/c.parquet")],
            "order verbatim (b before a), 1-based line numbers"
        );
    }

    #[test]
    fn manifest_multi_source_preserves_line_order() {
        let dir = tmpdir();
        // Deliberately list b before a: order must be kept, not sorted.
        write_standard(&dir.path().join("b.parquet"), vec![0, 1], false);
        write_standard(&dir.path().join("a.parquet"), vec![2, 3], false);
        let manifest = dir.path().join("parts.txt");
        std::fs::write(
            &manifest,
            format!(
                "# two partitions, b first\n{}\n\n{}\n",
                dir.path().join("b.parquet").display(),
                dir.path().join("a.parquet").display()
            ),
        )
        .unwrap();
        let src = ConvertSource::from_manifest(&manifest).unwrap();
        assert!(matches!(src, ConvertSource::Multi(_)));
        let plan = ReadPlan {
            batch_size: 1024,
            projection: None,
            row_groups: None,
        };
        assert_eq!(stream_ids(&src, &plan), vec![0, 1, 2, 3]);
    }

    /// A single-entry manifest collapses to `Single`; a `file://` URL line
    /// proves the URL classification path runs per line (no network).
    #[test]
    fn manifest_single_entry_and_file_url() {
        let dir = tmpdir();
        let f = dir.path().join("only.parquet");
        write_standard(&f, vec![7], false);
        let manifest = dir.path().join("one.txt");
        std::fs::write(&manifest, format!("file://{}\n", f.display())).unwrap();
        let src = ConvertSource::from_manifest(&manifest).unwrap();
        assert!(matches!(src, ConvertSource::Single(_)));
        let plan = ReadPlan {
            batch_size: 8,
            projection: None,
            row_groups: None,
        };
        assert_eq!(stream_ids(&src, &plan), vec![7]);
    }

    /// A manifest line naming a missing local file errors, naming BOTH the
    /// line number and the offending path. Lines are single files only —
    /// a directory line is rejected too (no recursion).
    #[test]
    fn manifest_missing_file_names_line_and_path() {
        let dir = tmpdir();
        write_standard(&dir.path().join("ok.parquet"), vec![1], false);
        let manifest = dir.path().join("bad.txt");
        std::fs::write(
            &manifest,
            format!(
                "{}\n# comment\n{}\n",
                dir.path().join("ok.parquet").display(),
                dir.path().join("nope.parquet").display()
            ),
        )
        .unwrap();
        let err = ConvertSource::from_manifest(&manifest).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("line 3"), "names the manifest line: {msg}");
        assert!(msg.contains("nope.parquet"), "names the path: {msg}");

        // A directory entry is not a file: same error (no recursion).
        let manifest2 = dir.path().join("dir.txt");
        std::fs::write(&manifest2, format!("{}\n", dir.path().display())).unwrap();
        let err = ConvertSource::from_manifest(&manifest2).unwrap_err();
        assert!(
            matches!(err, InputError::MissingListedInput { .. }),
            "directory lines are rejected: {err:?}"
        );
    }

    #[test]
    fn manifest_empty_or_unreadable_is_a_clear_error() {
        let dir = tmpdir();
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "# only comments\n\n").unwrap();
        let err = ConvertSource::from_manifest(&empty).unwrap_err();
        assert!(matches!(err, InputError::NoParquetInputs { .. }), "{err:?}");

        let err = ConvertSource::from_manifest(&dir.path().join("missing.txt")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("missing.txt"), "names the manifest: {msg}");
    }

    /// The Python list-input path: explicit entries, order verbatim, each a
    /// single file (no recursion), missing entries named by position.
    #[test]
    fn input_list_preserves_order_and_validates() {
        let dir = tmpdir();
        write_standard(&dir.path().join("b.parquet"), vec![0], false);
        write_standard(&dir.path().join("a.parquet"), vec![1], false);
        let list = [
            dir.path().join("b.parquet").display().to_string(),
            dir.path().join("a.parquet").display().to_string(),
        ];
        let src = ConvertSource::from_input_list(&list).unwrap();
        let plan = ReadPlan {
            batch_size: 8,
            projection: None,
            row_groups: None,
        };
        assert_eq!(stream_ids(&src, &plan), vec![0, 1]);

        let bad = [dir.path().join("nope.parquet").display().to_string()];
        let err = ConvertSource::from_input_list(&bad).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("entry 1"), "names the entry: {msg}");
        assert!(msg.contains("nope.parquet"), "names the path: {msg}");

        let none: [String; 0] = [];
        let err = ConvertSource::from_input_list(&none).unwrap_err();
        assert!(matches!(err, InputError::NoParquetInputs { .. }), "{err:?}");
    }

    // --- remote prefix listing (v0.7 PR-B) ------------------------------------

    #[cfg(feature = "remote")]
    mod remote_listing {
        use super::*;
        use crate::input::remote::{list_parquet_under_prefix, RemoteSource};
        use object_store::memory::InMemory;
        use object_store::path::Path as ObjectPath;
        use object_store::{ObjectStore, ObjectStoreExt};

        /// Seed one `InMemory` store with `objects` (key → bytes).
        fn seeded_store(objects: &[(&str, Vec<u8>)]) -> Arc<InMemory> {
            let store = Arc::new(InMemory::new());
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            for (key, bytes) in objects {
                rt.block_on(store.put(&ObjectPath::from(*key), bytes.clone().into()))
                    .unwrap();
            }
            store
        }

        fn parquet_bytes(ids: Vec<i64>) -> Vec<u8> {
            let batch = RecordBatch::try_new(
                Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false)])),
                vec![Arc::new(Int64Array::from(ids))],
            )
            .unwrap();
            let mut buf = Vec::new();
            let mut w = ArrowWriter::try_new(&mut buf, batch.schema(), None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            buf
        }

        /// The listing finds exactly the `.parquet` keys, sorted by key,
        /// skipping `_SUCCESS`, zero-byte objects, hidden (`.`/`_`) names —
        /// including hidden "directory" components below the prefix — and
        /// keys outside the prefix.
        #[test]
        fn listing_filters_and_sorts() {
            let store = seeded_store(&[
                ("set/b.parquet", parquet_bytes(vec![1])),
                ("set/a.parquet", parquet_bytes(vec![2])),
                ("set/nested/c.parquet", parquet_bytes(vec![3])),
                ("set/_SUCCESS", vec![]),
                ("set/_delta_log/d.parquet", parquet_bytes(vec![4])),
                ("set/.hidden.parquet", parquet_bytes(vec![5])),
                ("set/zero.parquet", vec![]),
                ("set/readme.txt", b"hi".to_vec()),
                ("other/x.parquet", parquet_bytes(vec![6])),
            ]);
            let store: Arc<dyn ObjectStore> = store;
            let listed =
                list_parquet_under_prefix(&store, &ObjectPath::from("set"), "memory://set/")
                    .unwrap();
            let keys: Vec<String> = listed
                .iter()
                .map(|p| p.location.as_ref().to_string())
                .collect();
            assert_eq!(
                keys,
                vec!["set/a.parquet", "set/b.parquet", "set/nested/c.parquet"],
                "exactly the visible .parquet keys, sorted"
            );
            assert!(listed.iter().all(|p| p.size > 0), "sizes from the listing");
        }

        /// Listed parts stream in key order through a `ConvertSource`, all
        /// sharing ONE store instance.
        #[test]
        fn listed_parts_stream_in_order() {
            let store = seeded_store(&[
                ("set/p1.parquet", parquet_bytes(vec![10, 11])),
                ("set/p0.parquet", parquet_bytes(vec![0, 1])),
            ]);
            let store: Arc<dyn ObjectStore> = store;
            let listed =
                list_parquet_under_prefix(&store, &ObjectPath::from("set"), "memory://set/")
                    .unwrap();
            let parts: Vec<InputSource> = listed
                .into_iter()
                .map(|p| {
                    InputSource::Remote(RemoteSource::from_store_meta(
                        Arc::clone(&store),
                        format!("memory://{}", p.location),
                        p,
                    ))
                })
                .collect();
            let src = ConvertSource::Multi(
                MultiSource::from_sources("memory://set/".to_string(), parts).unwrap(),
            );
            let plan = ReadPlan {
                batch_size: 1024,
                projection: None,
                row_groups: None,
            };
            assert_eq!(stream_ids(&src, &plan), vec![0, 1, 10, 11]);
            let stats = src.fetch_stats().expect("remote parts have stats");
            assert!(stats.object_size > 0, "object_size sums the parts");
        }

        /// PR-C: explicit-list (manifest / input-list) entries resolve under
        /// bounded parallelism, but the resulting parts MUST preserve entry
        /// order VERBATIM (the row-order invariant) — never completion or
        /// lexicographic order.
        #[test]
        #[expect(
            clippy::disallowed_types,
            reason = "#779: test observes concurrent per-call worker progress"
        )]
        fn explicit_list_parallel_connect_preserves_order() {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Condvar, Mutex as StdMutex};

            let store = seeded_store(&[
                ("m/a.parquet", parquet_bytes(vec![10])),
                ("m/b.parquet", parquet_bytes(vec![0, 1])),
                ("m/c.parquet", parquet_bytes(vec![20])),
            ]);
            let store: Arc<dyn ObjectStore> = store;

            // Deliberately NOT lexicographic: c, a, b.
            let entries: Vec<(String, String)> = ["m/c.parquet", "m/a.parquet", "m/b.parquet"]
                .into_iter()
                .enumerate()
                .map(|(i, key)| (format!("entry {}", i + 1), key.to_string()))
                .collect();

            // Overlap detector: the first resolver call blocks until a
            // second one is in flight (or a generous timeout passes), so a
            // serial implementation fails the `overlapped` assertion while
            // a parallel one passes deterministically.
            let in_flight = StdMutex::new(0usize);
            let cv = Condvar::new();
            let overlapped = AtomicBool::new(false);

            let resolved = resolve_list_entries(&entries, |_context, entry| {
                {
                    let mut n = in_flight.lock().unwrap();
                    *n += 1;
                    if *n >= 2 {
                        overlapped.store(true, Ordering::SeqCst);
                        cv.notify_all();
                    } else {
                        let (guard, _) = cv
                            .wait_timeout_while(n, std::time::Duration::from_secs(10), |_| {
                                !overlapped.load(Ordering::SeqCst)
                            })
                            .unwrap();
                        drop(guard);
                    }
                }
                Ok(InputSource::Remote(RemoteSource::from_store(
                    Arc::clone(&store),
                    ObjectPath::from(entry),
                    format!("memory://{entry}"),
                )?))
            })
            .unwrap();
            assert!(
                overlapped.load(Ordering::SeqCst),
                "entry connects must overlap (bounded parallelism), not run serially"
            );

            let src = ConvertSource::from_parts("list", resolved).unwrap();
            let plan = ReadPlan {
                batch_size: 1024,
                projection: None,
                row_groups: None,
            };
            assert_eq!(
                stream_ids(&src, &plan),
                vec![20, 10, 0, 1],
                "parts follow entry order verbatim"
            );
        }

        /// Parallel completion order must not change WHICH error surfaces:
        /// the first failing entry by INDEX wins.
        #[test]
        fn explicit_list_first_error_by_index_wins() {
            let entries: Vec<(String, String)> = (1..=4)
                .map(|i| (format!("entry {i}"), format!("e{i}")))
                .collect();
            let err = resolve_list_entries(&entries, |context, entry| {
                if entry == "e2" || entry == "e4" {
                    return Err(InputError::MissingListedInput {
                        context: context.to_string(),
                        input: entry.to_string(),
                    });
                }
                Ok(InputSource::Local(PathBuf::from(entry)))
            })
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("entry 2"),
                "first failing entry by index wins: {msg}"
            );
        }
    }

    // --- layer-name derivation (v0.7 PR-C) ------------------------------------

    /// Single files (local or nonexistent-yet paths) keep the historical
    /// behavior: the file stem, `"layer"` when there is none.
    #[test]
    fn layer_name_single_file_is_stem() {
        assert_eq!(derive_layer_name("/data/buildings.parquet"), "buildings");
        assert_eq!(derive_layer_name("buildings.parquet"), "buildings");
        // A --files-from manifest path takes the manifest file's stem.
        assert_eq!(
            derive_layer_name("/tmp/portland-roads.txt"),
            "portland-roads"
        );
    }

    /// Directory inputs use the directory's last path segment VERBATIM
    /// (no extension stripping — a dotted directory name is a name, not
    /// a file extension), trailing slash tolerated.
    #[test]
    fn layer_name_directory_is_last_segment() {
        let dir = tmpdir();
        let parts = dir.path().join("nyc_buildings");
        std::fs::create_dir(&parts).unwrap();
        assert_eq!(derive_layer_name(parts.to_str().unwrap()), "nyc_buildings");

        let dotted = dir.path().join("buildings.v2");
        std::fs::create_dir(&dotted).unwrap();
        assert_eq!(
            derive_layer_name(&format!("{}/", dotted.display())),
            "buildings.v2"
        );
    }

    /// Glob inputs use the deepest literal (wildcard-free) path segment
    /// before the first wildcard segment; an all-wildcard pattern falls
    /// back to `"layer"`.
    #[test]
    fn layer_name_glob_uses_last_literal_segment() {
        assert_eq!(derive_layer_name("/data/parts/*.parquet"), "parts");
        assert_eq!(derive_layer_name("/data/part-*.parquet"), "data");
        assert_eq!(derive_layer_name("/data/**/part-?.parquet"), "data");
        assert_eq!(derive_layer_name("*.parquet"), "layer");
    }

    /// `s3://`/`gs://` prefixes use the last non-empty path segment of the
    /// prefix (query string and fragment stripped); a bucket-root prefix
    /// uses the bucket name.
    #[test]
    fn layer_name_remote_prefix_uses_last_segment() {
        assert_eq!(derive_layer_name("s3://bucket/datasets/roads/"), "roads");
        assert_eq!(derive_layer_name("gs://bucket/roads/"), "roads");
        assert_eq!(derive_layer_name("s3://bucket/roads/?list-type=2"), "roads");
        assert_eq!(derive_layer_name("s3://bucket/"), "bucket");
    }

    /// Remote single objects behave like local single files: the object
    /// key's stem, with query string / fragment stripped first.
    #[test]
    fn layer_name_remote_object_is_stem() {
        assert_eq!(derive_layer_name("s3://bucket/data/roads.parquet"), "roads");
        assert_eq!(
            derive_layer_name("https://host/dl/roads.parquet?sig=abc"),
            "roads"
        );
        // Extension-less presigned/API URL: last segment verbatim.
        assert_eq!(derive_layer_name("https://host/api/download/42"), "42");
    }

    /// Degenerate inputs sanitize to the historical `"layer"` fallback.
    #[test]
    fn layer_name_degenerate_falls_back() {
        assert_eq!(derive_layer_name(""), "layer");
        assert_eq!(derive_layer_name("s3://"), "layer");
        assert_eq!(derive_layer_name("/"), "layer");
    }

    // --- #553: stream position -> file row ---------------------------------

    /// Without a selection, a single part's stream row IS its file row, and
    /// a second part's rows restart at 0.
    #[test]
    fn stream_row_locator_maps_parts_without_a_selection() {
        let counts = vec![vec![3, 2], vec![4]];
        let loc = StreamRowLocator::new(&counts, None);
        assert_eq!(loc.locate(0), Some((0, 0)));
        assert_eq!(loc.locate(4), Some((0, 4)));
        assert_eq!(loc.locate(5), Some((1, 0)));
        assert_eq!(loc.locate(8), Some((1, 3)));
        assert_eq!(loc.locate(9), None, "past the end of the stream");
    }

    /// A pruned row group shifts every later stream position; the locator
    /// adds the pruned group's rows back, and skips empty row groups and
    /// empty part selections the way the reader does.
    #[test]
    fn stream_row_locator_restores_rows_of_pruned_row_groups() {
        // Part 0: groups of 3, 0, 2, 4 rows; groups 0 and 1 pruned.
        // Part 1: groups of 5, 5; nothing selected.
        // Part 2: groups of 2, 2; group 1 selected.
        let counts = vec![vec![3, 0, 2, 4], vec![5, 5], vec![2, 2]];
        let sel = RowGroupSelection::from_parts(vec![vec![1, 2, 3], vec![], vec![1]]);
        let loc = StreamRowLocator::new(&counts, Some(&sel));
        // Stream rows 0..2 are part 0's group 2 (file rows 3..5).
        assert_eq!(loc.locate(0), Some((0, 3)));
        assert_eq!(loc.locate(1), Some((0, 4)));
        // Stream rows 2..6 are part 0's group 3 (file rows 5..9).
        assert_eq!(loc.locate(2), Some((0, 5)));
        assert_eq!(loc.locate(5), Some((0, 8)));
        // Stream rows 6..8 are part 2's group 1 (file rows 2..4).
        assert_eq!(loc.locate(6), Some((2, 2)));
        assert_eq!(loc.locate(7), Some((2, 3)));
        assert_eq!(loc.locate(8), None);
    }

    // --- #563: byte-array batch ceiling -------------------------------------

    /// The largest column is the one with the most bytes summed over the
    /// selected row groups, counting only the roots the caller asks for.
    #[test]
    fn largest_byte_array_column_sums_the_selected_groups_and_counted_roots() {
        use arrow_array::BinaryArray;

        let dir = tmpdir();
        let f = dir.path().join("geo.parquet");
        // Row group 0: two 16-byte geometries. Row group 1: two 4096-byte
        // geometries. `name` holds 1000 bytes per row in both groups, and
        // `empty` holds only empty strings.
        let blobs: Vec<Vec<u8>> = [16, 16, 4096, 4096].map(|n| vec![0u8; n]).into();
        let refs: Vec<&[u8]> = blobs.iter().map(Vec::as_slice).collect();
        let names: Vec<String> = (0..4).map(|_| "n".repeat(1000)).collect();
        write_parquet(
            &f,
            vec![
                Field::new("geometry", DataType::Binary, false),
                Field::new("name", DataType::Utf8, false),
                Field::new("empty", DataType::Utf8, false),
            ],
            vec![
                Arc::new(BinaryArray::from_vec(refs)),
                Arc::new(StringArray::from(names)),
                Arc::new(StringArray::from(vec![""; 4])),
            ],
            Some(2),
        );
        let meta = tylertoo_io::read_parquet_metadata(&std::fs::File::open(&f).unwrap()).unwrap();
        let meta = meta.as_ref();

        let all = tylertoo_io::largest_byte_array_column(meta, None, |_| true).unwrap();
        assert_eq!(all.column, "geometry");
        assert!(
            (8224..8224 + 128).contains(&all.total_bytes),
            "the sum over both groups: {all:?}"
        );

        let small = tylertoo_io::largest_byte_array_column(meta, Some(&[0]), |_| true).unwrap();
        assert_eq!(small.column, "name", "2000 name bytes beat 32: {small:?}");

        let names_only =
            tylertoo_io::largest_byte_array_column(meta, None, |root| root == 1).unwrap();
        assert_eq!(names_only.column, "name");
        assert!(tylertoo_io::largest_byte_array_column(meta, None, |_| false).is_none());
        assert!(
            tylertoo_io::largest_byte_array_column(meta, None, |root| root == 2).is_none(),
            "a column holding no bytes is not a largest column"
        );
    }
}

/// Remote staging counters (#788 track R: #400, #727, #544). Each test pins
/// one count the single staging plan must hit; on `origin/main` (cb1bdfc) the
/// first four fail and the last passes (it is the premise #544 rests on).
#[cfg(all(test, feature = "remote"))]
mod staging_counter_tests {
    use super::*;
    use crate::input::probe_store::ProbeStore;
    use arrow_array::{Int64Array, StringArray};
    use arrow_schema::DataType;
    use std::ops::Range;
    use std::time::Duration;
    use tylertoo_io::test_support::{ArrowWriter, WriterProperties};

    const COALESCE_VAR: &str = "TYLERTOO_STAGING_COALESCE_BYTES";
    const CONCURRENCY_VAR: &str = "TYLERTOO_STAGING_CONCURRENCY";

    /// Runs the named test of this module alone in a child process with
    /// `vars` set, so the process-wide staging tunables (read once from the
    /// environment) are exactly these. Returns `true` inside that child,
    /// where the caller runs its body, and `false` in the parent once the
    /// child has passed. Same shape as `run_in_child` in `input::remote`.
    fn run_in_child(test: &str, vars: &[(&str, &str)]) -> bool {
        const CHILD: &str = "TYLERTOO_ENV_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        let (_, module) = module_path!().split_once("::").expect("crate::module path");
        let name = format!("{module}::{test}");
        let out = std::process::Command::new(std::env::current_exe().expect("test binary path"))
            .args([name.as_str(), "--exact", "--test-threads=1", "--nocapture"])
            .env(CHILD, "1")
            .envs(vars.iter().copied())
            .output()
            .expect("re-run the test binary");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "{name} failed in its own process:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        false
    }

    /// `groups` row groups of `rows` rows, columns `a: Int64`, `b: Utf8`,
    /// `c: Int64` (roots 0, 1, 2), so `b`'s chunk sits between the other two
    /// in every row group.
    fn three_column_parquet(rows: usize, groups: usize) -> Vec<u8> {
        let n = rows * groups;
        let schema = Arc::new(Schema::new(vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Utf8, false),
            Field::new("c", DataType::Int64, false),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from((0..n as i64).collect::<Vec<_>>())),
                Arc::new(StringArray::from(
                    (0..n).map(|i| format!("row-{i:08}")).collect::<Vec<_>>(),
                )),
                Arc::new(Int64Array::from(
                    (0..n as i64).map(|i| i * 7).collect::<Vec<_>>(),
                )),
            ],
        )
        .unwrap();
        let props = WriterProperties::builder()
            .set_max_row_group_row_count(Some(rows))
            .build();
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
        buf
    }

    /// Byte range of every column chunk, as `[row_group][root]` (the schema is
    /// flat, so leaf index == root index).
    fn chunk_ranges(bytes: &[u8]) -> Vec<Vec<Range<u64>>> {
        let md = tylertoo_io::read_parquet_metadata(&bytes::Bytes::from(bytes.to_vec())).unwrap();
        md.row_groups()
            .iter()
            .map(|rg| {
                rg.columns()
                    .iter()
                    .map(|c| {
                        let (s, len) = c.byte_range();
                        s..s + len
                    })
                    .collect()
            })
            .collect()
    }

    /// Requests and bytes `f` adds to `src`'s fetch counters.
    fn counted(src: &ConvertSource, f: impl FnOnce()) -> (u64, u64, Vec<Range<u64>>) {
        let before = src.fetch_stats().unwrap();
        let ranges_before: usize = src
            .parts()
            .iter()
            .map(|p| p.fetched_ranges().unwrap().len())
            .sum();
        f();
        let after = src.fetch_stats().unwrap();
        let ranges: Vec<Range<u64>> = src
            .parts()
            .iter()
            .flat_map(|p| p.fetched_ranges().unwrap())
            .skip(ranges_before)
            .collect();
        (
            after.requests - before.requests,
            after.bytes_fetched - before.bytes_fetched,
            ranges,
        )
    }

    /// Read every selected row of `src` through its (projected) stream.
    fn read_all(src: &ConvertSource) -> usize {
        let plan = ReadPlan {
            batch_size: 1024,
            projection: None,
            row_groups: None,
        };
        src.open_stream(&plan)
            .unwrap()
            .map(|b| b.unwrap().num_rows())
            .sum()
    }

    /// #400: under a property selection, staging fetches only the chunks of
    /// the kept columns. Main fetches every chunk of every selected row group
    /// (one span per row group): 5 requests, all bytes.
    #[test]
    fn staging_fetches_only_the_projected_column_chunks() {
        if !run_in_child(
            "staging_fetches_only_the_projected_column_chunks",
            &[(COALESCE_VAR, "0")],
        ) {
            return;
        }
        let bytes = three_column_parquet(1000, 5);
        let chunks = chunk_ranges(&bytes);
        let needed: u64 = chunks
            .iter()
            .flat_map(|rg| [&rg[0], &rg[2]])
            .map(|r| r.end - r.start)
            .sum();
        let excluded: Vec<Range<u64>> = chunks.iter().map(|rg| rg[1].clone()).collect();

        let src = ConvertSource::single(crate::input::test_memory_source(bytes, "p.parquet"));
        src.restrict_columns(vec![0, 2]).unwrap();
        src.num_row_groups_total().unwrap(); // footer first, so it is not counted

        let (requests, fetched, ranges) = counted(&src, || src.stage_selected(None).unwrap());
        for r in &ranges {
            assert!(
                excluded
                    .iter()
                    .all(|x| r.end <= x.start || r.start >= x.end),
                "staging fetched {r:?}, which overlaps an excluded column chunk"
            );
        }
        assert_eq!(
            fetched, needed,
            "staging must fetch the kept chunks' {needed} bytes, nothing else"
        );
        // Window 0: one request per contiguous run of kept chunks in a row
        // group; `b` splits each row group into `a` and `c`.
        assert_eq!(requests, 10, "two kept runs in each of five row groups");

        // And the staged chunks serve the whole projected read.
        let (after, _, _) = counted(&src, || assert_eq!(read_all(&src), 5000));
        assert_eq!(after, 0, "the projected read must come from the spill");
    }

    /// #400 / C1's interface: `bytes_to_stage` is the projected footprint of
    /// the selected row groups of the remote parts. Without a projection it
    /// equals the selected compressed bytes; a local source stages nothing.
    #[test]
    fn bytes_to_stage_is_the_projected_remote_footprint() {
        let bytes = three_column_parquet(1000, 5);
        let chunks = chunk_ranges(&bytes);
        let len = |r: &Range<u64>| r.end - r.start;
        let kept: u64 = chunks.iter().flat_map(|rg| [&rg[0], &rg[2]]).map(len).sum();
        let kept_rg1: u64 = len(&chunks[1][0]) + len(&chunks[1][2]);

        let full =
            ConvertSource::single(crate::input::test_memory_source(bytes.clone(), "f.parquet"));
        assert_eq!(
            full.bytes_to_stage(None).unwrap(),
            full.selected_input_bytes(None).unwrap()
        );
        let src =
            ConvertSource::single(crate::input::test_memory_source(bytes.clone(), "b.parquet"));
        src.restrict_columns(vec![0, 2]).unwrap();
        assert_eq!(src.bytes_to_stage(None).unwrap(), kept);
        let sel = RowGroupSelection::from_parts(vec![vec![1]]);
        assert_eq!(src.bytes_to_stage(Some(&sel)).unwrap(), kept_rg1);
        // Planning is footer-only: nothing was fetched past the footer.
        let (requests, _, _) = counted(&src, || {
            src.bytes_to_stage(None).unwrap();
        });
        assert_eq!(requests, 0);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("l.parquet");
        std::fs::write(&path, &bytes).unwrap();
        let local = ConvertSource::resolve(path.to_str().unwrap()).unwrap();
        assert_eq!(
            local.bytes_to_stage(None).unwrap(),
            0,
            "local parts never stage"
        );
    }

    /// #400 + #290: a coalescing window merges byte-adjacent kept chunks
    /// across row groups (`c` of one group and `a` of the next) but never
    /// bridges an excluded chunk, so the bytes stay the projected bytes.
    /// Main merges all five row groups, excluded chunks included, into ONE
    /// request of every byte.
    #[test]
    fn a_window_merges_kept_chunks_but_never_fetches_an_excluded_one() {
        if !run_in_child(
            "a_window_merges_kept_chunks_but_never_fetches_an_excluded_one",
            &[(COALESCE_VAR, "268435456")],
        ) {
            return;
        }
        let bytes = three_column_parquet(1000, 5);
        let chunks = chunk_ranges(&bytes);
        let needed: u64 = chunks
            .iter()
            .flat_map(|rg| [&rg[0], &rg[2]])
            .map(|r| r.end - r.start)
            .sum();
        // Sanity on the fixture: `c` of group i ends where `a` of i+1 starts.
        for w in chunks.windows(2) {
            assert_eq!(w[0][2].end, w[1][0].start, "fixture layout changed");
        }

        let src = ConvertSource::single(crate::input::test_memory_source(bytes, "w.parquet"));
        src.restrict_columns(vec![0, 2]).unwrap();
        src.num_row_groups_total().unwrap();

        let (requests, fetched, _) = counted(&src, || src.stage_selected(None).unwrap());
        assert_eq!(fetched, needed, "only the kept chunks' bytes");
        // a0 | c0+a1 | c1+a2 | c2+a3 | c3+a4 | c4
        assert_eq!(requests, 6, "kept runs merged across row-group seams");
    }

    /// #727 §2: parts stage concurrently under one in-flight budget. Four
    /// parts of two row groups each (eight spans) at the default ceiling of
    /// eight: every span is in flight at once. Main stages one part at a
    /// time, so at most that part's two spans are ever in flight.
    #[test]
    fn staging_keeps_spans_of_several_parts_in_flight_at_once() {
        if !run_in_child(
            "staging_keeps_spans_of_several_parts_in_flight_at_once",
            &[(COALESCE_VAR, "0"), (CONCURRENCY_VAR, "8")],
        ) {
            return;
        }
        let objects: Vec<(String, Vec<u8>)> = (0..4)
            .map(|i| (format!("part-{i}.parquet"), three_column_parquet(500, 2)))
            .collect();
        let named: Vec<(&str, Vec<u8>)> = objects
            .iter()
            .map(|(n, b)| (n.as_str(), b.clone()))
            .collect();
        let store = ProbeStore::new(&named, Duration::from_millis(100));
        let parts = objects.iter().map(|(n, _)| store.source(n)).collect();
        let src = ConvertSource::from_parts("memory://", parts).unwrap();
        src.num_row_groups_total().unwrap();
        store.take_max_in_flight(); // footers do not count

        let (requests, _, _) = counted(&src, || src.stage_selected(None).unwrap());
        assert_eq!(requests, 8, "one span per row group");
        assert_eq!(
            store.take_max_in_flight(),
            8,
            "all eight spans, across four parts, must be in flight together"
        );
    }

    /// #727 §1: one failed request costs only its own chunks. Part 0's GETs
    /// all fail; parts 1 and 2 are still staged, and their reads then need no
    /// network. Main returns on the first error, so parts 1 and 2 are never
    /// staged (zero requests).
    #[test]
    fn a_failed_part_does_not_stop_the_other_parts_staging() {
        if !run_in_child(
            "a_failed_part_does_not_stop_the_other_parts_staging",
            &[(COALESCE_VAR, "0")],
        ) {
            return;
        }
        let objects: Vec<(String, Vec<u8>)> = (0..3)
            .map(|i| (format!("f-{i}.parquet"), three_column_parquet(500, 2)))
            .collect();
        let named: Vec<(&str, Vec<u8>)> = objects
            .iter()
            .map(|(n, b)| (n.as_str(), b.clone()))
            .collect();
        let store = ProbeStore::new(&named, Duration::ZERO);
        let parts = objects.iter().map(|(n, _)| store.source(n)).collect();
        let src = ConvertSource::from_parts("memory://", parts).unwrap();
        src.num_row_groups_total().unwrap();
        store.fail_next_gets("f-0.parquet", usize::MAX);
        let before: Vec<u64> = src
            .parts()
            .iter()
            .map(|p| p.fetch_stats().unwrap().requests)
            .collect();

        let _ = src.stage_selected(None); // best-effort: the verdict is the counters
        assert!(store.failed() > 0, "the injected failure must have fired");
        for (i, part) in src.parts().iter().enumerate().skip(1) {
            assert_eq!(
                part.fetch_stats().unwrap().requests - before[i],
                2,
                "part {i} must be staged (one request per row group) despite part 0 failing"
            );
        }
    }

    /// #727 §1: a coalesced request that fails is retried as its row groups
    /// instead of abandoning staging. One merged span of five row groups fails
    /// once; staging still succeeds and the read needs no network. Main
    /// returns the error and stages nothing.
    #[test]
    fn a_failed_coalesced_span_is_retried_per_row_group() {
        if !run_in_child(
            "a_failed_coalesced_span_is_retried_per_row_group",
            &[(COALESCE_VAR, "268435456")],
        ) {
            return;
        }
        let bytes = three_column_parquet(1000, 5);
        let store = ProbeStore::new(&[("r.parquet", bytes)], Duration::ZERO);
        let src = ConvertSource::single(store.source("r.parquet"));
        src.num_row_groups_total().unwrap();
        store.fail_next_gets("r.parquet", 1);

        let (requests, _, _) = counted(&src, || {
            src.stage_selected(None)
                .expect("one transient failure must not fail staging");
        });
        assert_eq!(store.failed(), 1);
        assert_eq!(
            requests, 5,
            "the failed merged span, retried as 5 row groups"
        );
        let (after, _, _) = counted(&src, || assert_eq!(read_all(&src), 5000));
        assert_eq!(after, 0, "every row group staged after the retry");
    }

    /// #544's premise, which already holds on main: once every selected
    /// chunk is staged, concurrent readers over disjoint row groups of one
    /// remote part are served from the spill and never touch the network,
    /// so the shared chunk cache cannot make them evict each other into
    /// re-fetches. What keeps pass 2 single-threaded is the read layer's
    /// `!source.is_remote()` gate, not this.
    #[test]
    fn staged_remote_rows_serve_concurrent_readers_without_the_network() {
        let bytes = three_column_parquet(1000, 4);
        let src = ConvertSource::single(crate::input::test_memory_source(bytes, "c.parquet"));
        src.num_row_groups_total().unwrap();
        src.stage_selected(None).unwrap();
        let (requests, _, _) = counted(&src, || {
            std::thread::scope(|s| {
                let handles: Vec<_> = (0..4)
                    .map(|rg| {
                        let src = &src;
                        s.spawn(move || {
                            let sel = RowGroupSelection::from_parts(vec![vec![rg]]);
                            let plan = ReadPlan {
                                batch_size: 128,
                                projection: None,
                                row_groups: Some(&sel),
                            };
                            src.open_stream(&plan)
                                .unwrap()
                                .map(|b| b.unwrap().num_rows())
                                .sum::<usize>()
                        })
                    })
                    .collect();
                let rows: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
                assert_eq!(rows, 4000);
            });
        });
        assert_eq!(requests, 0, "four concurrent readers, all from the spill");
    }
}
