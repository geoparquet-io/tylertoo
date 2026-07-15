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
use parquet::arrow::arrow_reader::ParquetRecordBatchReader;
use parquet::arrow::ProjectionMask;
use parquet::file::metadata::{KeyValue, ParquetMetaData};

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

/// One parquet file/object plus its lazily cached footer metadata, so the
/// header phase's accessors (schema, kv metadata, row-group counts,
/// selection) parse the footer ONCE — matching the pre-v0.7 single-open
/// header cost.
#[derive(Debug)]
pub struct SingleSource {
    source: InputSource,
    meta: std::sync::OnceLock<PartMeta>,
}

impl SingleSource {
    fn new(source: InputSource) -> Self {
        SingleSource {
            source,
            meta: std::sync::OnceLock::new(),
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
    /// Arrow schema decoded from the part's footer.
    schema: SchemaRef,
    /// Parsed parquet metadata (row groups, key-value metadata).
    parquet: Arc<ParquetMetaData>,
}

/// An ordered set of parquet partitions (local files and/or remote
/// objects) with footers loaded and validated up front (see the module
/// docs for the compatibility rules).
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
    ///   [`InputError::RemoteDisabled`] as before);
    /// - `http(s)://` prefix → [`InputError::RemotePrefixUnsupported`]
    ///   (no generic listing API; the error points at `--files-from`);
    /// - a single resolved partition collapses to `Single`;
    /// - an empty directory/glob result → [`InputError::NoParquetInputs`].
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
    /// of surfacing as a bare I/O error at footer-load time.
    fn from_explicit_list(root: &str, entries: &[(String, String)]) -> Result<Self, InputError> {
        let mut parts = Vec::with_capacity(entries.len());
        for (context, entry) in entries {
            let src = InputSource::from_str_input(entry)?;
            // Without the `remote` feature `Local` is the only variant.
            #[cfg_attr(not(feature = "remote"), allow(irrefutable_let_patterns))]
            if let InputSource::Local(p) = &src {
                if !p.is_file() {
                    return Err(InputError::MissingListedInput {
                        context: context.clone(),
                        input: entry.clone(),
                    });
                }
            }
            parts.push(src);
        }
        Self::from_parts(root, parts)
    }

    /// [`ConvertSource::resolve`] for `Path` inputs (non-UTF-8 paths fall
    /// back to a single local source, as [`InputSource::from_path`] does).
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
    /// validated union schema (nullability OR-ed across parts).
    pub fn schema(&self) -> Result<SchemaRef, InputError> {
        match self {
            ConvertSource::Single(s) => Ok(s.meta()?.schema.clone()),
            ConvertSource::Multi(m) => Ok(m.schema.clone()),
        }
    }

    /// Parquet key-value metadata of partition 0 (the `geo` metadata used
    /// for CRS detection; construction validated all parts agree).
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
    Ok(PartMeta {
        schema: builder.schema().clone(),
        parquet: builder.metadata().clone(),
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
    /// ([`FOOTER_LOAD_CONCURRENCY`]): a remote footer is two range
    /// requests, so hundreds of parts would otherwise serialize hundreds
    /// of round-trips — but must not open hundreds of connections at once
    /// either.
    pub fn from_sources(root: String, parts: Vec<InputSource>) -> Result<Self, InputError> {
        assert!(!parts.is_empty(), "MultiSource requires at least one part");
        let metas = load_part_metas(&parts)?;

        let first_name = parts[0].display_name();
        // The reference CRS: `Ok(crs)` per detect_crs_from_kv, `None` for an
        // unsupported/undetectable CRS. Parts must AGREE; supportedness
        // itself is enforced by the pipeline (against part 0), so a set
        // that consistently carries one unsupported CRS still errors with
        // the standard UnsupportedCrs message.
        let crs_of = |m: &PartMeta| {
            crate::overview::convert::detect_crs_from_kv(
                m.parquet.file_metadata().key_value_metadata(),
            )
            .ok()
        };
        // Raw CRS descriptor from the `geo` metadata: disambiguates two
        // *different* unsupported CRSs, which both detect to `None` and
        // would otherwise "agree" here only to error later naming just
        // part 0's CRS.
        let raw_crs_of = |m: &PartMeta| -> String {
            crate::quality::crs_info_from_kv_metadata(
                m.parquet.file_metadata().key_value_metadata(),
            )
            .ok()
            .and_then(|info| info.identifier.or(info.name))
            .unwrap_or_else(|| "unknown".to_string())
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

/// Parse `--files-from` manifest text into `(1-based line number, entry)`
/// pairs: entries are trimmed; blank lines and lines whose first non-space
/// character is `#` are skipped. Order is preserved verbatim (see
/// [`ConvertSource::from_manifest`]).
fn manifest_entries(text: &str) -> Vec<(usize, &str)> {
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
#[cfg_attr(not(feature = "remote"), allow(dead_code))]
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
    /// for real now (covered by the InMemory tests below), so they are NOT
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
            writer.append_key_value_metadata(parquet::file::metadata::KeyValue::new(
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
        use object_store::ObjectStore;

        /// Seed one InMemory store with `objects` (key → bytes).
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

        /// Listed parts stream in key order through a ConvertSource, all
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
                    InputSource::Remote(RemoteSource::from_store_sized(
                        Arc::clone(&store),
                        p.location.clone(),
                        format!("memory://{}", p.location),
                        p.size,
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
}
