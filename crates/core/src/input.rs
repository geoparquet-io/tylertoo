//! Input-source abstraction: local files or remote objects (issue #210).
//!
//! The overview converter historically opened its input with
//! [`std::fs::File`]. This module generalizes that to an [`InputSource`]
//! that can also point at an object in remote storage (`s3://`, `https://`,
//! `http://`, `gs://`), served to the existing *synchronous* parquet reader
//! plumbing through the [`tylertoo_io::ChunkReader`] trait:
//!
//! - the parquet footer is fetched with range requests,
//! - each column chunk of each *selected* row group is fetched as ONE byte
//!   range, the first time the sync reader touches it (the buffered
//!   range-fetch adapter: the page reader's many small header/page reads
//!   are served from the whole-chunk buffer).
//!
//! Fetches never extend past a column chunk **by design**: only chunks the
//! parquet reader actually touches are requested. This is what makes the
//! composition with `--bbox` row-group pruning (#102 / PR #207) the headline
//! feature — pruned row groups are never requested at all, so a city-scale
//! extract from a country-scale remote file moves only a fraction of the
//! object's bytes. [`InputSource::fetch_stats`] exposes request/byte
//! counters so callers (and tests) can verify that property.
//!
//! The streaming pipeline re-reads the input across several passes (assign,
//! coarse levels, finest streamed last). Each [`InputSource::open`] of a
//! remote source reuses a cached parsed footer, and fetched column chunks are
//! served from the cheapest tier that holds them:
//!
//! - **L1**, a bounded in-memory cache (insertion-order eviction) sized to the
//!   largest row group's working set (floored at
//!   `remote::CHUNK_CACHE_MAX_BYTES`), so a row group larger than the floor
//!   does not thrash — the fix for the per-page re-fetch of an oversized column
//!   chunk (issue #261);
//! - **L2**, a local on-disk spill of every chunk ever fetched, so a chunk
//!   evicted from L1 between passes is drained from local disk instead of the
//!   network — this bounds remote traffic to ≈1× the object regardless of pass
//!   or level count (issue #219; without it a full-file remote convert moved
//!   ~3× the object);
//! - **L3**, one network range request, taken only on the first touch of a
//!   chunk.
//!
//! See `docs/remote-reads.md` for the fetch-count implications.
//!
//! Remote support is compiled behind the `remote` cargo feature; the CLI and
//! Python bindings enable it by default. Without the feature, URL inputs
//! fail with a clear `InputError::RemoteDisabled` error.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use serde::Serialize;
use tylertoo_io::ParquetError;
use tylertoo_io::ReaderMetadata;
use tylertoo_io::{ChunkReader, Length};

use tylertoo_io::ParquetSource;

/// Schemes recognized as remote inputs (when the `remote` feature is on).
const REMOTE_SCHEMES: &[&str] = &["s3", "s3a", "http", "https", "gs"];

/// Errors from opening or reading an [`InputSource`].
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// A saved convert plan belongs to a different version of an input.
    #[error("--plan: saved plan does not match this run: input {input:?} {field} was {saved:?} when the plan was saved but is {current:?} now. Re-run without --plan (add --save-plan to write a fresh one).")]
    PlanInputChanged {
        /// Input path or URL.
        input: String,
        /// Identity field that changed.
        field: &'static str,
        /// Value stored in the plan.
        saved: String,
        /// Value observed for this run.
        current: String,
    },
    /// A checked-I/O boundary refused the input.
    #[error(transparent)]
    Decode(#[from] tylertoo_io::Error),
    /// Local I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Parquet error (footer parse, reader construction).
    #[error("parquet error: {}", parquet_error_detail(.0))]
    Parquet(#[from] ParquetError),
    /// An input file could not be opened (#452: the bare [`Self::Io`] named
    /// no file). Not a `#[source]`: the message already carries the reason,
    /// and a source would repeat it in every `Caused by:` chain.
    #[error("cannot open {input}: {error}")]
    Open {
        /// Display name of the input (path or URL).
        input: String,
        /// The underlying I/O error.
        error: std::io::Error,
    },
    /// An input's Parquet footer could not be read or fetched (#452: the
    /// bare [`Self::Parquet`] named no file).
    #[error("cannot read the Parquet footer of {input}: {}", parquet_error_detail(.error))]
    Footer {
        /// Display name of the input (path or URL).
        input: String,
        /// The underlying Parquet error.
        error: ParquetError,
    },
    /// The input looks like a URL but uses a scheme we do not support.
    #[error(
        "unsupported input URL scheme `{scheme}://` in {url:?}: supported inputs are \
         local paths and s3://, https://, http://, gs:// URLs"
    )]
    UnsupportedScheme {
        /// The unrecognized scheme.
        scheme: String,
        /// The full input string.
        url: String,
    },
    /// A remote URL was supplied but the crate was built without `remote`.
    #[cfg(not(feature = "remote"))]
    #[error(
        "remote input {0:?} requires tylertoo-core's `remote` feature (the official \
         CLI and Python builds enable it; rebuild with `--features remote`)"
    )]
    RemoteDisabled(String),
    /// The remote object store rejected a request.
    #[cfg(feature = "remote")]
    #[error("remote input error for {url}: {source}")]
    Remote {
        /// The input URL.
        url: String,
        /// The underlying object-store error.
        #[source]
        source: object_store::Error,
    },
    /// Remote configuration problem (bad URL, missing region, ...).
    #[cfg(feature = "remote")]
    #[error("{0}")]
    RemoteConfig(String),
    /// Arrow error surfaced while decoding record batches from a stream.
    #[error("arrow error: {0}")]
    Arrow(#[from] arrow_schema::ArrowError),
    /// A partition of a multi-file input does not match the first partition
    /// (schema shape, geometry extension metadata, or CRS).
    #[error("incompatible input partition {offender:?}: {detail} (first partition: {first:?})")]
    IncompatiblePartition {
        /// Display name of the reference (first) partition.
        first: String,
        /// Display name of the partition that failed validation.
        offender: String,
        /// What differs.
        detail: String,
    },
    /// A directory or glob input matched no `.parquet` files.
    #[error("no .parquet files found for input {input:?}")]
    NoParquetInputs {
        /// The original input string (directory path or glob pattern).
        input: String,
    },
    /// An `http(s)://` URL naming a prefix ("directory"). Generic HTTP has
    /// no listing API, so the objects must be named explicitly; `s3://` and
    /// `gs://` prefixes are listed natively.
    #[error(
        "cannot list objects under {url:?}: http(s) prefixes have no generic \
         listing API. Pass the object URLs explicitly with --files-from \
         <manifest> (one per line); s3:// and gs:// prefixes are listed \
         natively"
    )]
    RemotePrefixUnsupported {
        /// The rejected URL.
        url: String,
    },
    /// The `--files-from` manifest itself could not be read.
    #[error("cannot read --files-from manifest {path:?}: {source}")]
    ManifestRead {
        /// The manifest path as supplied.
        path: String,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// An explicitly listed input (a `--files-from` manifest line or a
    /// Python input-list entry) does not exist as a local file. Listed
    /// entries are single files/objects only — no directory, glob, or
    /// prefix expansion.
    #[error("{context}: input {input:?} does not exist or is not a file")]
    MissingListedInput {
        /// Where the entry came from (manifest line / list position).
        context: String,
        /// The offending entry.
        input: String,
    },
    /// A glob input string is not a valid glob pattern.
    #[error("invalid glob pattern {pattern:?}: {message}")]
    GlobPattern {
        /// The pattern as supplied.
        pattern: String,
        /// The glob crate's error message.
        message: String,
    },
}

/// A [`ParquetError`]'s message without the `Parquet error: ` prefix its
/// `Display` adds to [`ParquetError::General`], so a message that already
/// says what failed does not say "Parquet error" twice (#452).
pub(crate) fn parquet_error_detail(error: &ParquetError) -> String {
    match error {
        ParquetError::General(message) => message.clone(),
        // The owner's typed errors (#779) travel as `External`: show them,
        // not parquet's "External: " prefix.
        ParquetError::External(inner) => inner.to_string(),
        other => other.to_string(),
    }
}

/// Byte/request counters for a remote input. `Serialize` so conversion
/// reports can carry it (benchmark tasks).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Default)]
pub struct FetchStats {
    /// Number of range GET requests issued (HEAD not included).
    pub requests: u64,
    /// Total bytes fetched across all range requests.
    pub bytes_fetched: u64,
    /// Total size of the remote object in bytes.
    pub object_size: u64,
}

/// A conversion input: a local parquet file or a remote parquet object.
///
/// `Clone` is cheap: the local variant clones a path, the remote variant
/// clones `Arc` handles — clones share the fetch counters, chunk cache,
/// disk spill, and cached footer of the original.
#[derive(Debug, Clone)]
pub enum InputSource {
    /// A local filesystem path (the historical behavior).
    Local(PathBuf),
    /// A remote object read over range requests.
    #[cfg(feature = "remote")]
    Remote(remote::RemoteSource),
}

impl InputSource {
    /// Classify a CLI-style input path. Anything shaped like `scheme://...`
    /// is treated as a URL (`file://` maps back to a local path); everything
    /// else is a local path.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if the path is a URL with an unsupported
    /// scheme, a remote URL in a build without the `remote` feature, or a
    /// remote URL the client cannot connect to.
    pub fn from_path(path: &Path) -> Result<Self, InputError> {
        let Some(s) = path.to_str() else {
            // Non-UTF-8 paths cannot be URLs; treat as local.
            return Ok(InputSource::Local(path.to_path_buf()));
        };
        Self::from_str_input(s)
    }

    /// [`InputSource::from_path`] for string inputs.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if `input` is a URL with an unsupported
    /// scheme, a remote URL in a build without the `remote` feature, or a
    /// remote URL the client cannot connect to.
    pub fn from_str_input(input: &str) -> Result<Self, InputError> {
        let Some(scheme) = url_scheme(input) else {
            return Ok(InputSource::Local(PathBuf::from(input)));
        };
        if scheme.eq_ignore_ascii_case("file") {
            let rest = &input[scheme.len() + 3..];
            return Ok(InputSource::Local(PathBuf::from(rest)));
        }
        if !REMOTE_SCHEMES
            .iter()
            .any(|s| scheme.eq_ignore_ascii_case(s))
        {
            return Err(InputError::UnsupportedScheme {
                scheme: scheme.to_string(),
                url: input.to_string(),
            });
        }
        #[cfg(feature = "remote")]
        {
            Ok(InputSource::Remote(remote::RemoteSource::connect(input)?))
        }
        #[cfg(not(feature = "remote"))]
        {
            Err(InputError::RemoteDisabled(input.to_string()))
        }
    }

    /// Whether this source is remote.
    pub fn is_remote(&self) -> bool {
        match self {
            InputSource::Local(_) => false,
            #[cfg(feature = "remote")]
            InputSource::Remote(_) => true,
        }
    }

    /// Metadata captured when the remote object was opened or listed.
    /// Local files continue to use filesystem metadata for plan binding.
    pub fn remote_meta(&self) -> Option<&tylertoo_io::RemoteObjectMeta> {
        match self {
            Self::Local(_) => None,
            #[cfg(feature = "remote")]
            Self::Remote(source) => Some(&source.object_meta),
        }
    }

    /// Human-readable name of the input (path or URL).
    pub fn display_name(&self) -> String {
        match self {
            InputSource::Local(p) => p.display().to_string(),
            #[cfg(feature = "remote")]
            InputSource::Remote(r) => r.url().to_string(),
        }
    }

    /// Open a parquet reader builder over this input.
    ///
    /// Local: opens the file and parses the footer (cheap, OS page cache).
    /// Remote: reuses a cached parsed footer after the first open, so the
    /// multi-pass streaming pipeline pays the footer fetch only once.
    ///
    /// Callers that open the same input REPEATEDLY — the pass-2 reader
    /// segments, above all — should use `Self::open_with_metadata` instead:
    /// a local footer parse is cheap once and quadratic in row groups times
    /// columns when paid per reader.
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if the file cannot be opened or its Parquet
    /// footer cannot be read or fetched.
    pub fn open(&self) -> Result<ParquetSource<InputReader>, InputError> {
        match self {
            InputSource::Local(p) => {
                tylertoo_io::open_parquet(self.open_local(p)?).map_err(|error| InputError::Footer {
                    input: self.display_name(),
                    error,
                })
            }
            #[cfg(feature = "remote")]
            InputSource::Remote(r) => r.open_builder(),
        }
    }

    fn open_local(&self, path: &Path) -> Result<InputReader, InputError> {
        File::open(path)
            .map(InputReader::Local)
            .map_err(|error| InputError::Open {
                input: self.display_name(),
                error,
            })
    }

    /// Open a parquet reader builder over this input using ALREADY-PARSED
    /// footer metadata.
    ///
    /// "Cheap, OS page cache" is true of the file *read*, not of the parse:
    /// `ParquetMetaDataReader` decodes every row group's every column chunk
    /// descriptor, so a footer parse costs `O(row_groups` × columns) and the
    /// thrift decode of a many-row-group footer is measurable — 6.5-19 s for
    /// 1084 segment opens on the #494 Brazil fixture, paid once per reader
    /// opened. The caller already holds the parsed footer
    /// (`input_set::PartMeta`), so hand it over instead.
    ///
    /// Remote inputs ignore `meta` and take [`Self::open`]'s path: it already
    /// caches its `ReaderMetadata`, and opening also re-sizes the chunk
    /// cache from the footer, which is load-bearing (#261).
    pub(crate) fn open_with_metadata(
        &self,
        meta: &ReaderMetadata,
    ) -> Result<ParquetSource<InputReader>, InputError> {
        match self {
            InputSource::Local(p) => Ok(tylertoo_io::open_parquet_with_metadata(
                self.open_local(p)?,
                meta.clone(),
            )),
            #[cfg(feature = "remote")]
            InputSource::Remote(r) => r.open_builder(),
        }
    }

    /// Fetch counters for a remote source (`None` for local inputs).
    pub fn fetch_stats(&self) -> Option<FetchStats> {
        match self {
            InputSource::Local(_) => None,
            #[cfg(feature = "remote")]
            InputSource::Remote(r) => Some(r.fetch_stats()),
        }
    }

    /// The byte ranges fetched so far from a remote source (`None` for
    /// local inputs). Ordered by request time; used by tests to prove that
    /// bbox-pruned row groups are never downloaded.
    pub fn fetched_ranges(&self) -> Option<Vec<std::ops::Range<u64>>> {
        match self {
            InputSource::Local(_) => None,
            #[cfg(feature = "remote")]
            InputSource::Remote(r) => Some(r.fetched_ranges()),
        }
    }

    /// Place the remote-input disk spill (#219) in `dir` instead of the
    /// process temp dir (`$TMPDIR`) — issue #272. No-op for local inputs,
    /// which never spill. Call before reading: chunks already spilled stay
    /// in the previously created file; only the spill-file creation (lazy,
    /// on the first spilled chunk) honors the directory.
    pub fn set_spill_dir(&self, dir: Option<&Path>) {
        #[cfg(not(feature = "remote"))]
        let _ = dir;
        #[cfg(feature = "remote")]
        if let InputSource::Remote(r) = self {
            r.set_spill_dir(dir);
        }
    }

    /// Release the in-memory read cache (no-op for local files). For a
    /// remote source this clears the L1 chunk cache but KEEPS the disk
    /// spill and the cached footer, so a later touch of the same chunk is
    /// served from local disk, not the network. Called by multi-partition
    /// streams on part transitions to bound resident memory to one part's
    /// working set.
    pub fn release_read_cache(&self) {
        match self {
            InputSource::Local(_) => {}
            #[cfg(feature = "remote")]
            InputSource::Remote(r) => r.release_read_cache(),
        }
    }

    /// Stage the selected row groups to the local disk spill up front (pass
    /// 0, #286/#287), coalescing each row group into one parallel range
    /// request so the later passes read entirely from disk. No-op for local
    /// inputs (the OS page cache already serves re-reads). `selected` (`None`
    /// = all) must match the row groups the passes will read, so pruned groups
    /// are never fetched and total traffic stays ≈1× (#219).
    ///
    /// # Errors
    ///
    /// Returns an [`InputError`] if fetching a remote row group or writing it
    /// to the spill fails.
    pub fn stage_row_groups(&self, selected: Option<&[usize]>) -> Result<(), InputError> {
        #[cfg(not(feature = "remote"))]
        let _ = selected;
        match self {
            InputSource::Local(_) => Ok(()),
            #[cfg(feature = "remote")]
            InputSource::Remote(r) => r.stage_row_groups(selected),
        }
    }
}

/// Sum of the compressed byte sizes of the selected input row groups.
///
/// These are the bytes a remote convert will touch, and therefore the projected size of
/// the disk spill (#219): every touched chunk is staged locally exactly
/// once, so the spill grows to ≈ this number (issue #272 free-space
/// preflight). `None` selects every row group (a full-file read); indices
/// out of range are ignored. Single-file shape on purpose (metadata +
/// selection → bytes): a multi-partition source sums it across parts.
pub fn selected_compressed_bytes(
    metadata: &tylertoo_io::ParquetMetaData,
    selected_row_groups: Option<&[usize]>,
) -> u64 {
    selected_groups(metadata, selected_row_groups)
        .map(row_group_compressed_bytes)
        .fold(0, u64::saturating_add)
}

/// Iterate a footer selection in its supplied order, ignoring invalid indices.
/// With no selection, visit every row group. Counts and byte estimates share
/// these semantics for both single-file and multipart inputs.
pub(crate) fn selected_groups<'a>(
    metadata: &'a tylertoo_io::ParquetMetaData,
    selected: Option<&'a [usize]>,
) -> impl Iterator<Item = &'a tylertoo_io::RowGroupMetaData> {
    let groups = metadata.row_groups();
    let len = selected.map_or(groups.len(), <[usize]>::len);
    (0..len).filter_map(move |i| groups.get(selected.map_or(i, |picked| picked[i])))
}

/// A row group's compressed bytes as its footer declares them, saturating.
///
/// Not parquet-rs's `RowGroupMetaData::compressed_size`: that sums the
/// chunks' `i64` sizes unchecked, and a footer can declare any size (#686).
/// A negative chunk size counts as nothing.
pub(crate) fn row_group_compressed_bytes(rg: &tylertoo_io::RowGroupMetaData) -> u64 {
    rg.columns()
        .iter()
        .map(|col| col.compressed_size().max(0) as u64)
        .fold(0, u64::saturating_add)
}

/// Whether `scheme` is one of the recognized remote URL schemes.
#[cfg(feature = "remote")]
pub(crate) fn is_remote_scheme(scheme: &str) -> bool {
    REMOTE_SCHEMES
        .iter()
        .any(|s| scheme.eq_ignore_ascii_case(s))
}

/// Return the URL scheme of `input` if it is shaped like `scheme://rest`.
pub(crate) fn url_scheme(input: &str) -> Option<&str> {
    let (scheme, _) = input.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let mut chars = scheme.chars();
    let first = chars.next()?;
    if !first.is_ascii_alphabetic() {
        return None;
    }
    if chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')) {
        Some(scheme)
    } else {
        None
    }
}

/// A [`ChunkReader`] over an [`InputSource`]: the type the parquet reader
/// plumbing is instantiated with.
#[derive(Debug)]
pub enum InputReader {
    /// Local file (delegates to parquet's own `File` impl).
    Local(File),
    /// Remote object; every `get_bytes` is one range request.
    #[cfg(feature = "remote")]
    Remote(remote::RemoteReader),
}

impl Length for InputReader {
    fn len(&self) -> u64 {
        match self {
            InputReader::Local(f) => f.len(),
            #[cfg(feature = "remote")]
            InputReader::Remote(r) => r.object_size(),
        }
    }
}

impl ChunkReader for InputReader {
    type T = Box<dyn Read + Send>;

    fn get_read(&self, start: u64) -> Result<Self::T, ParquetError> {
        match self {
            InputReader::Local(f) => Ok(Box::new(f.get_read(start)?)),
            #[cfg(feature = "remote")]
            InputReader::Remote(r) => Ok(Box::new(r.sequential_reader(start))),
        }
    }

    fn get_bytes(&self, start: u64, length: usize) -> Result<Bytes, ParquetError> {
        match self {
            InputReader::Local(f) => f.get_bytes(start, length),
            #[cfg(feature = "remote")]
            InputReader::Remote(r) => r.get_bytes_range(start, length),
        }
    }
}

#[cfg(feature = "remote")]
pub(crate) mod remote {
    //! Remote object-store backend (`remote` feature).

    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::ops::Range;
    use std::path::{Path, PathBuf};
    #[expect(
        clippy::disallowed_types,
        reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
    )]
    use std::sync::atomic::{AtomicU64, Ordering};
    #[expect(
        clippy::disallowed_types,
        reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
    )]
    use std::sync::{Arc, Mutex, OnceLock};

    use bytes::Bytes;
    use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey, AwsCredential};
    use object_store::gcp::{GoogleCloudStorageBuilder, GoogleConfigKey};
    use object_store::http::HttpBuilder;
    use object_store::path::Path as ObjectPath;
    // ObjectStoreExt: object_store 0.14 moved `get_range` and `head` off the
    // base trait onto an extension trait.
    use object_store::{
        ClientConfigKey, ClientOptions, CredentialProvider, ObjectStore, ObjectStoreExt,
        ObjectStoreScheme,
    };
    use tylertoo_io::ParquetError;
    use tylertoo_io::ReaderMetadata;
    use url::Url;

    use super::{FetchStats, InputError, InputReader};

    /// Readahead for the *sequential* read path
    /// ([`tylertoo_io::ChunkReader::get_read`]). The page reader
    /// uses `get_read` only to thrift-decode page *headers* (tens of bytes;
    /// page data goes through exact-range `get_bytes`), so keep this small —
    /// and always clamp it to the surrounding column chunk (see
    /// [`SharedState::clamp_to_chunk`]) so a header read near a chunk
    /// boundary can never pull bytes from a bbox-pruned row group.
    const SEQUENTIAL_CHUNK: u64 = 8 * 1024;

    /// Shared tokio runtime driving `object_store`'s async I/O from our
    /// synchronous reader plumbing. Two worker threads: requests are issued
    /// one at a time per reader, so this only needs to run the HTTP client.
    fn runtime() -> &'static tokio::runtime::Runtime {
        #[expect(
            clippy::disallowed_types,
            reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
        )]
        static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("gpq-remote-io")
                .enable_all()
                .build()
                .expect("failed to build tokio runtime for remote input")
        })
    }

    /// State shared by all readers of one source: request/byte counters, the
    /// fetched-range log, and (once the footer is parsed) the column-chunk
    /// byte ranges used to clamp sequential readahead.
    #[derive(Debug, Default)]
    #[expect(
        clippy::disallowed_types,
        reason = "#779: per-operation caches, worker coordination and profiling; no diagnostic globals"
    )]
    struct SharedState {
        requests: AtomicU64,
        bytes: AtomicU64,
        ranges: Mutex<Vec<Range<u64>>>,
        /// Byte ranges of every column chunk, sorted by start; populated
        /// from the parsed footer on first open.
        chunk_ranges: OnceLock<Vec<Range<u64>>>,
    }

    impl SharedState {
        /// Clamp a readahead starting at `start` so it never crosses out of
        /// the column chunk containing `start` (a page-header read must not
        /// bleed into a neighboring — possibly bbox-pruned — row group).
        fn clamp_to_chunk(&self, start: u64, want_end: u64) -> u64 {
            let Some(chunks) = self.chunk_ranges.get() else {
                return want_end;
            };
            // Last chunk starting at or before `start`.
            let idx = chunks.partition_point(|r| r.start <= start);
            if idx == 0 {
                return want_end;
            }
            let chunk = &chunks[idx - 1];
            if start < chunk.end {
                want_end.min(chunk.end)
            } else {
                want_end
            }
        }
    }

    /// A remote parquet object: store handle, resolved location, object
    /// size, fetch counters, and (after the first open) the parsed footer.
    ///
    /// `Clone` shares the counters, caches, spill, and cached footer via the
    /// inner `Arc`s (a clone is a second handle to the same object).
    #[derive(Debug, Clone)]
    pub struct RemoteSource {
        url: String,
        store: Arc<dyn ObjectStore>,
        location: ObjectPath,
        pub(super) object_meta: tylertoo_io::RemoteObjectMeta,
        shared: Arc<SharedState>,
        #[expect(
            clippy::disallowed_types,
            reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
        )]
        metadata: OnceLock<ReaderMetadata>,
        cache: Arc<Mutex<ChunkCache>>,
        /// On-disk overflow for fetched column chunks (issue #219), shared by
        /// every reader clone across the pipeline's passes so a chunk fetched
        /// in one pass is drained from local disk in the next.
        spill: Arc<Mutex<DiskSpill>>,
        /// Floor for the chunk-cache eviction budget. [`Self::open_builder`]
        /// raises the live cap to the largest row group's working set but
        /// never below this. Production uses [`CHUNK_CACHE_MAX_BYTES`]; tests
        /// inject a tiny value to exercise the eviction path at small scale.
        cap_base: u64,
    }

    /// Resolve the object store + object path for `url_str`.
    ///
    /// Stores are cached per `scheme://authority` for the life of the
    /// process, so one convert resolves credentials ONCE per bucket: a
    /// prefix listing and its N per-part sources, or N `--files-from`
    /// manifest entries in the same bucket, all share one store instance
    /// (the AWS credential chain probe in [`build_s3`] is otherwise paid
    /// per part).
    pub(crate) fn store_and_location(
        url_str: &str,
    ) -> Result<(Arc<dyn ObjectStore>, ObjectPath), InputError> {
        #[expect(
            clippy::disallowed_types,
            reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
        )]
        static STORES: OnceLock<Mutex<std::collections::HashMap<String, Arc<dyn ObjectStore>>>> =
            OnceLock::new();

        let url = Url::parse(url_str)
            .map_err(|e| InputError::RemoteConfig(format!("invalid URL {url_str:?}: {e}")))?;
        let (scheme, location) = ObjectStoreScheme::parse(&url).map_err(|e| {
            InputError::RemoteConfig(format!("cannot interpret URL {url_str:?}: {e}"))
        })?;
        let key = url[..url::Position::BeforePath].to_ascii_lowercase();
        let stores = STORES.get_or_init(Default::default);
        if let Some(store) = stores
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
        {
            return Ok((Arc::clone(store), location));
        }
        let overrides = effective_client_overrides();
        let store: Arc<dyn ObjectStore> = match scheme {
            ObjectStoreScheme::AmazonS3 => build_s3(&url)?,
            ObjectStoreScheme::GoogleCloudStorage => Arc::new(
                overrides
                    .iter()
                    .fold(
                        GoogleCloudStorageBuilder::from_env().with_url(url_str),
                        |b, (k, v)| b.with_config(GoogleConfigKey::Client(*k), v),
                    )
                    .build()
                    .map_err(|e| store_error(url_str, e))?,
            ),
            ObjectStoreScheme::Http => {
                let origin = &url[..url::Position::BeforePath];
                Arc::new(
                    overrides
                        .iter()
                        .fold(
                            HttpBuilder::new()
                                .with_url(origin)
                                .with_client_options(ClientOptions::new().with_allow_http(true)),
                            |b, (k, v)| b.with_config(*k, v),
                        )
                        .build()
                        .map_err(|e| store_error(url_str, e))?,
                )
            }
            _ => {
                return Err(InputError::UnsupportedScheme {
                    scheme: url.scheme().to_string(),
                    url: url_str.to_string(),
                })
            }
        };
        // A concurrent resolve may have raced us; either instance works,
        // last insert wins.
        stores
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, Arc::clone(&store));
        Ok((store, location))
    }

    /// List the `.parquet` objects under remote prefix `prefix`, applying
    /// the same hygiene as the local directory walk
    /// ([`crate::input_set::list_parquet_files`]): keys must end
    /// `.parquet`; zero-byte objects and any path component below the
    /// prefix starting with `.` or `_` (`_SUCCESS`, `_delta_log/…`,
    /// `.crc`) are skipped; results are sorted by key — the ordering the
    /// converter's row-order invariant keys on.
    pub(crate) fn list_parquet_under_prefix(
        store: &Arc<dyn ObjectStore>,
        prefix: &ObjectPath,
        url_str: &str,
    ) -> Result<Vec<object_store::ObjectMeta>, InputError> {
        use futures_util::TryStreamExt;
        let metas: Vec<object_store::ObjectMeta> = runtime()
            .block_on(store.list(Some(prefix)).try_collect())
            .map_err(|e| store_error(url_str, e))?;
        let prefix_depth = prefix.parts().count();
        let mut parts: Vec<object_store::ObjectMeta> = metas
            .into_iter()
            .filter(|m| {
                m.size > 0
                    && m.location.as_ref().ends_with(".parquet")
                    && m.location
                        .parts()
                        .skip(prefix_depth)
                        .all(|c| !c.as_ref().starts_with(['.', '_']))
            })
            .collect();
        parts.sort_by(|a, b| a.location.as_ref().cmp(b.location.as_ref()));
        Ok(parts)
    }

    /// Connect every listed `.parquet` object under remote prefix
    /// `url_str`, in key order. ONE store instance serves the listing and
    /// all returned sources (one credential resolution), and the object
    /// sizes come from the listing itself — no per-part HEAD requests.
    pub(crate) fn sources_under_prefix(url_str: &str) -> Result<Vec<RemoteSource>, InputError> {
        let (store, prefix) = store_and_location(url_str)?;
        let parts = list_parquet_under_prefix(&store, &prefix, url_str)?;
        Ok(parts
            .into_iter()
            .map(|p| {
                let url = part_url(url_str, &p.location);
                RemoteSource::from_store_meta(Arc::clone(&store), url, p)
            })
            .collect())
    }

    /// URL of one listed object: the prefix URL's `scheme://authority`
    /// plus the object's full key.
    fn part_url(prefix_url: &str, location: &ObjectPath) -> String {
        match Url::parse(prefix_url) {
            Ok(u) => format!("{}/{}", &u[..url::Position::BeforePath], location),
            Err(_) => format!("{prefix_url}{location}"),
        }
    }

    impl RemoteSource {
        /// Connect to `url`, resolving the backing store from the scheme
        /// (cached per bucket, see [`store_and_location`]) and HEAD-ing the
        /// object for its size.
        pub(crate) fn connect(url_str: &str) -> Result<Self, InputError> {
            let (store, location) = store_and_location(url_str)?;
            Self::from_store(store, location, url_str.to_string())
        }

        /// Build a source over an explicit store + location (also the test
        /// seam: unit tests inject [`object_store::memory::InMemory`]).
        pub fn from_store(
            store: Arc<dyn ObjectStore>,
            location: ObjectPath,
            url: String,
        ) -> Result<Self, InputError> {
            Self::from_store_with_cap_base(store, location, url, CHUNK_CACHE_MAX_BYTES)
        }

        /// [`Self::from_store`] with an explicit chunk-cache floor. Tests pass
        /// a tiny `cap_base` so a small fixture's row group exceeds it, which
        /// exercises the eviction path (and the #261 refetch pathology) at
        /// unit-test scale instead of needing a >256 MiB object.
        pub(crate) fn from_store_with_cap_base(
            store: Arc<dyn ObjectStore>,
            location: ObjectPath,
            url: String,
            cap_base: u64,
        ) -> Result<Self, InputError> {
            let head = runtime()
                .block_on(store.head(&location))
                .map_err(|e| store_error(&url, e))?;
            Ok(Self::from_store_meta_with_cap_base(
                store, url, head, cap_base,
            ))
        }

        /// [`Self::from_store`] with object metadata already known (from a
        /// prefix listing) — skips the HEAD request, so connecting N listed
        /// parts costs no network round-trips at all.
        pub(crate) fn from_store_meta(
            store: Arc<dyn ObjectStore>,
            url: String,
            meta: object_store::ObjectMeta,
        ) -> Self {
            Self::from_store_meta_with_cap_base(store, url, meta, CHUNK_CACHE_MAX_BYTES)
        }

        fn from_store_meta_with_cap_base(
            store: Arc<dyn ObjectStore>,
            url: String,
            meta: object_store::ObjectMeta,
            cap_base: u64,
        ) -> Self {
            Self {
                url,
                store,
                location: meta.location.clone(),
                object_meta: meta.into(),
                shared: Arc::new(SharedState::default()),
                #[expect(
                    clippy::disallowed_types,
                    reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
                )]
                metadata: OnceLock::new(),
                cache: Arc::new(Mutex::new(ChunkCache::new(cap_base))),
                spill: Arc::new(Mutex::new(DiskSpill::default())),
                cap_base,
            }
        }

        /// The input URL.
        pub fn url(&self) -> &str {
            &self.url
        }

        /// Open a parquet reader builder; the parsed footer is cached across
        /// opens so multi-pass pipelines fetch it once.
        pub(crate) fn open_builder(
            &self,
        ) -> Result<tylertoo_io::ParquetSource<InputReader>, InputError> {
            let reader = InputReader::Remote(self.reader());
            let metadata = self.load_footer()?;
            // Column-chunk byte ranges (sorted) for readahead clamping.
            // `load_footer` has already rejected any chunk whose range
            // overflows or runs past the object (#430), so the add cannot
            // wrap; saturate anyway rather than trust a cached footer.
            self.shared.chunk_ranges.get_or_init(|| {
                let mut ranges: Vec<Range<u64>> = metadata
                    .metadata()
                    .row_groups()
                    .iter()
                    .flat_map(|rg| {
                        rg.columns().iter().map(|col| {
                            let (start, len) = col.byte_range();
                            start..start.saturating_add(len)
                        })
                    })
                    .collect();
                ranges.sort_by_key(|r| r.start);
                ranges
            });
            // Size the chunk-cache eviction budget to the largest row group's
            // working set (#261). The arrow reader interleaves a row group's
            // projected column chunks across batches, so if their combined size
            // exceeds the cache the reader thrashes — a chunk is evicted and
            // re-fetched on the next batch. A remote input whose geometry
            // column chunk alone dwarfs the 256 MiB floor then re-fetches that
            // chunk on every page read (measured 96× on fieldmaps-adm4, whose
            // 3 row groups each carry a 1.3 GiB geometry chunk). Holding one
            // row group's chunks resident bounds the fetch to ≈ 1× per pass;
            // memory stays O(largest row group), which the whole-chunk fetch
            // already materializes to serve a range.
            let max_row_group: u64 = metadata
                .metadata()
                .row_groups()
                .iter()
                .map(crate::input::row_group_compressed_bytes)
                .max()
                .unwrap_or(0);
            {
                let mut cache = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                cache.cap = self.cap_base.max(max_row_group);
            }
            Ok(tylertoo_io::open_parquet_with_metadata(reader, metadata))
        }

        /// A cheap handle for issuing counted range requests.
        pub(crate) fn reader(&self) -> RemoteReader {
            RemoteReader {
                store: Arc::clone(&self.store),
                location: self.location.clone(),
                object_meta: self.object_meta.clone(),
                shared: Arc::clone(&self.shared),
                cache: Arc::clone(&self.cache),
                spill: Arc::clone(&self.spill),
            }
        }

        /// Place the disk spill's (anonymous) file in `dir` — issue #272,
        /// see [`InputSource::set_spill_dir`]. Shared by every reader clone.
        ///
        /// [`InputSource::set_spill_dir`]: super::InputSource::set_spill_dir
        pub(crate) fn set_spill_dir(&self, dir: Option<&Path>) {
            self.spill
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .dir = dir.map(Path::to_path_buf);
        }

        /// Parse (and cache) the footer, returning the shared metadata.
        /// Mirrors the load in [`Self::open_builder`] so staging (pass 0) does
        /// not depend on a builder open; the header phase has usually loaded
        /// it already, in which case this is a cheap clone with no fetch.
        fn load_footer(&self) -> Result<ReaderMetadata, InputError> {
            if let Some(md) = self.metadata.get() {
                return Ok(md.clone());
            }
            let reader = InputReader::Remote(self.reader());
            // The owner (#779) runs the footer pre-check, the #430 chunk-range
            // check against the object size and the guarded schema decode,
            // exactly as it does for a local file.
            let md =
                tylertoo_io::load_reader_metadata(&reader).map_err(|error| InputError::Footer {
                    input: self.url.clone(),
                    error,
                })?;
            // A concurrent open may have won the race; either copy is
            // equivalent.
            let _ = self.metadata.set(md.clone());
            Ok(md)
        }

        /// Pass 0 (#286/#287): stage the selected row groups to the disk spill
        /// up front — ONE coalesced range request per row group, several in
        /// flight at once — so both later passes read entirely from local
        /// disk.
        ///
        /// A row group's column chunks form a contiguous byte span, so each
        /// row group is fetched as a single large GET and sliced back into the
        /// per-column-chunk spill entries the reader's L2 path already serves
        /// from ([`RemoteReader::chunk_data`]). This removes the two
        /// latency-bound patterns the demo hit: the per-column-chunk, per-pass
        /// serial re-fetch (#287, reader kept ~1 request in flight), and in
        /// particular pass 2's cold re-fetch of the property columns that pass
        /// 1's geometry+ranking projection skipped (#286, ~10 small serial
        /// range requests per row group).
        ///
        /// `selected` (`None` = every row group) is the SAME per-part bbox
        /// row-group selection the passes read, so pruned row groups are still
        /// never touched and total network traffic stays ≈1× the object
        /// (#219); no speculative over-fetch. Each span counts as one request
        /// so callers observe the coalesced pattern. Best-effort by
        /// construction: a chunk that fails to spill (spill disabled / out of
        /// space, #272) simply falls back to the reader's network path on
        /// first touch, so a spill write is never fatal — only an actual fetch
        /// error, which the passes would hit anyway, is surfaced.
        pub(crate) fn stage_row_groups(
            &self,
            selected: Option<&[usize]>,
        ) -> Result<(), InputError> {
            self.stage_row_groups_with(selected, staging_tunables::effective())
        }

        /// [`Self::stage_row_groups`] with explicit tunables: the test seam,
        /// so a test can pin the window or concurrency without touching the
        /// process environment that [`staging_tunables::effective`] reads.
        fn stage_row_groups_with(
            &self,
            selected: Option<&[usize]>,
            tunables: &staging_tunables::StagingTunables,
        ) -> Result<(), InputError> {
            let metadata = self.load_footer()?;
            let row_groups = metadata.metadata().row_groups();

            let mut is_selected = vec![selected.is_none(); row_groups.len()];
            for &i in selected.unwrap_or_default() {
                if let Some(s) = is_selected.get_mut(i) {
                    *s = true;
                }
            }
            let mut plan: Vec<StagedSpan> = Vec::new();
            // Spans of the unselected row groups: a coalesced request must
            // never reach into one (#219/#210 — pruned bytes stay unfetched).
            let mut blocked: Vec<Range<u64>> = Vec::new();
            for (rg, &keep) in row_groups.iter().zip(&is_selected) {
                let mut start = u64::MAX;
                let mut end = 0u64;
                let mut chunks = Vec::with_capacity(rg.columns().len());
                for col in rg.columns() {
                    let (s, len) = col.byte_range();
                    // Checked (#430): `load_footer` validated these for every
                    // row group, pruned or not, but a wrapped span here would
                    // fetch the wrong bytes.
                    let chunk_end = s
                        .checked_add(len)
                        .filter(|e| *e <= self.object_meta.size)
                        .zip(usize::try_from(len).ok())
                        .ok_or_else(|| {
                            ParquetError::EOF(format!(
                                "column chunk range {s}..{} beyond object size {} for {}",
                                s as u128 + len as u128,
                                self.object_meta.size,
                                self.location
                            ))
                        })?;
                    start = start.min(s);
                    end = end.max(chunk_end.0);
                    chunks.push((s, chunk_end.1));
                }
                if end <= start {
                    continue;
                }
                if keep {
                    plan.push(StagedSpan {
                        span: start..end,
                        chunks,
                    });
                } else {
                    blocked.push(start..end);
                }
            }
            if plan.is_empty() {
                return Ok(());
            }
            let plan = coalesce_spans(plan, &blocked, tunables.coalesce_window);

            // Bound in-flight (resident) spans by the memory budget: holding
            // one row group transiently matches the L1 cache ceiling, so never
            // hold more than the budget's worth at once.
            let max_span = plan
                .iter()
                .map(|r| r.span.end - r.span.start)
                .max()
                .unwrap_or(0);
            let concurrency = match max_span {
                0 => 1,
                s => ((tunables.mem_budget / s).max(1) as usize).min(tunables.max_concurrency),
            };
            staging_tunables::log_live_concurrency(tunables, concurrency, max_span);
            log::debug!(
                "[stage] fetching {} span(s), {concurrency} in flight \
                 (largest span {max_span} B)",
                plan.len()
            );

            let store = Arc::clone(&self.store);
            let location = self.location.clone();
            let spill = Arc::clone(&self.spill);
            let shared = Arc::clone(&self.shared);
            let object_meta = self.object_meta.clone();

            runtime().block_on(async move {
                use futures_util::stream::StreamExt;
                let mut inflight = futures_util::stream::iter(plan.into_iter().map(|rg| {
                    let store = Arc::clone(&store);
                    let location = location.clone();
                    let object_meta = object_meta.clone();
                    async move {
                        let bytes = object_meta
                            .get_range(store.as_ref(), &location, rg.span.clone())
                            .await
                            .map_err(|e| ParquetError::External(Box::new(e)))?;
                        Ok::<(StagedSpan, Bytes), ParquetError>((rg, bytes))
                    }
                }))
                .buffer_unordered(concurrency);

                while let Some(result) = inflight.next().await {
                    let (rg, bytes) = result?;
                    // Count the coalesced request (mirrors `fetch`).
                    shared.requests.fetch_add(1, Ordering::Relaxed);
                    shared
                        .bytes
                        .fetch_add(bytes.len() as u64, Ordering::Relaxed);
                    shared
                        .ranges
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(rg.span.clone());
                    // Slice into per-column-chunk spill entries. Guard the
                    // slice bounds: a well-behaved store returns exactly the
                    // requested span, but a short read must fall back to the
                    // reader's network path (skip spilling), never panic.
                    let base = rg.span.start;
                    let mut spill = spill
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    for (chunk_start, len) in rg.chunks {
                        // Checked (#430): a wrapped `off + len` would pass the
                        // bound and panic inside `slice`.
                        let slice = chunk_start
                            .checked_sub(base)
                            .and_then(|off| usize::try_from(off).ok())
                            .and_then(|off| off.checked_add(len).map(|end| off..end))
                            .filter(|r| r.end <= bytes.len());
                        if let Some(r) = slice {
                            spill.put(chunk_start, &bytes.slice(r));
                        }
                    }
                }
                Ok::<(), ParquetError>(())
            })?;

            Ok(())
        }

        /// Snapshot of the fetch counters.
        pub fn fetch_stats(&self) -> FetchStats {
            FetchStats {
                requests: self.shared.requests.load(Ordering::Relaxed),
                bytes_fetched: self.shared.bytes.load(Ordering::Relaxed),
                object_size: self.object_meta.size,
            }
        }

        /// The byte ranges fetched so far, in request order.
        pub fn fetched_ranges(&self) -> Vec<Range<u64>> {
            self.shared
                .ranges
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }

        /// Drop the in-memory (L1) chunk cache. The disk spill (L2) and the
        /// cached footer are KEPT: a later pass re-reads spilled chunks from
        /// local disk, never the network. Multi-partition streams call this
        /// on part transitions so resident memory stays O(one part's row
        /// group) instead of O(parts × cap) (v0.7 multi-partition input).
        pub fn release_read_cache(&self) {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cache.entries.clear();
            cache.order.clear();
            cache.total = 0;
        }
    }

    /// Map an object-store error, attaching the input URL.
    fn store_error(url: &str, source: object_store::Error) -> InputError {
        store_error_with_hint(url, source, custom_endpoint_configured())
    }

    /// Whether a custom (non-AWS) S3 endpoint is configured via the env
    /// vars `AmazonS3Builder::from_env` honors.
    fn custom_endpoint_configured() -> bool {
        std::env::var_os("AWS_ENDPOINT_URL").is_some() || std::env::var_os("AWS_ENDPOINT").is_some()
    }

    /// Whether `text` (an object-store error rendering) looks like an S3
    /// signature/authorization failure — the shape an anonymous
    /// S3-compatible endpoint produces when ambient AWS credentials sign
    /// requests it cannot verify.
    fn looks_like_signature_error(text: &str) -> bool {
        const MARKERS: [&str; 5] = [
            "403",
            "forbidden",
            "invalidaccesskeyid",
            "signaturedoesnotmatch",
            "accessdenied",
        ];
        let lower = text.to_ascii_lowercase();
        MARKERS.iter().any(|m| lower.contains(m))
    }

    /// [`store_error`] with the endpoint check injected (test seam). With a
    /// custom endpoint configured, a signature/authorization-style failure
    /// appends the `AWS_SKIP_SIGNATURE=true` hint — string-level, in the
    /// Display path only (the error types are unchanged): resolved AWS
    /// credentials are signed into every request, and an anonymous
    /// S3-compatible endpoint rejects them with exactly this error shape.
    fn store_error_with_hint(
        url: &str,
        source: object_store::Error,
        custom_endpoint: bool,
    ) -> InputError {
        if custom_endpoint {
            let text = source.to_string();
            if looks_like_signature_error(&text) {
                return InputError::RemoteConfig(format!(
                    "remote input error for {url}: {text} — if this S3-compatible \
                     endpoint serves anonymous (unsigned) requests, retry with \
                     AWS_SKIP_SIGNATURE=true"
                ));
            }
        }
        InputError::Remote {
            url: url.to_string(),
            source,
        }
    }

    /// Build an S3 store for `url` with the standard AWS credential chain
    /// (env, shared config/credentials incl. `AWS_PROFILE`, SSO, IMDS —
    /// what DuckDB's `credential_chain` provider and gpio users expect).
    /// If the chain resolves no credentials the store falls back to
    /// unsigned requests, so public buckets work anonymously.
    fn build_s3(url: &Url) -> Result<Arc<dyn ObjectStore>, InputError> {
        use aws_credential_types::provider::ProvideCredentials;

        // `from_env` honors explicit AWS_* env overrides (region, endpoint,
        // static keys); `with_url` extracts the bucket (and, for
        // virtual-hosted https URLs, the region embedded in the host).
        let mut builder = AmazonS3Builder::from_env().with_url(url.to_string());

        let sdk_config =
            runtime().block_on(aws_config::defaults(aws_config::BehaviorVersion::latest()).load());

        // Region: explicit env (AWS_REGION / AWS_DEFAULT_REGION) wins, then
        // the profile/IMDS-resolved region from the SDK chain.
        let env_region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .ok();
        match (&env_region, sdk_config.region()) {
            (Some(r), _) => builder = builder.with_region(r.clone()),
            (None, Some(r)) => builder = builder.with_region(r.as_ref()),
            (None, None) => {
                return Err(InputError::RemoteConfig(format!(
                    "no AWS region configured for {url}: set AWS_REGION (e.g. \
                     AWS_REGION=us-east-2) or add a region to your AWS profile"
                )));
            }
        }

        // Credentials: probe the chain once; if it resolves, install a
        // refreshing bridge provider (SSO/STS credentials expire), else go
        // unsigned for public buckets.
        let mut anonymous = true;
        if let Some(provider) = sdk_config.credentials_provider() {
            match runtime().block_on(provider.provide_credentials()) {
                Ok(_) => {
                    builder = builder.with_credentials(Arc::new(SdkCredentialBridge(provider)));
                    anonymous = false;
                }
                Err(e) => {
                    log::warn!(
                        "no AWS credentials resolved ({e}); \
                         falling back to unsigned (anonymous) S3 requests"
                    );
                }
            }
        }
        if anonymous {
            builder = builder.with_skip_signature(true);
        }
        for (k, v) in effective_client_overrides() {
            builder = builder.with_config(AmazonS3ConfigKey::Client(*k), v);
        }

        Ok(Arc::new(builder.build().map_err(|e| {
            InputError::RemoteConfig(format!("cannot configure S3 store for {url}: {e}"))
        })?))
    }

    /// Prefix of the `object_store` client-option overrides (#290 dimension 4):
    /// `TYLERTOO_HTTP_<KEY>=<value>` sets client option `<key>` (lowercased,
    /// e.g. `TYLERTOO_HTTP_HTTP2_ONLY=true`, `TYLERTOO_HTTP_TIMEOUT=60s`) on
    /// every remote store, whatever the scheme, so one sweep knob works on
    /// both https and s3 hosts. Experimental, like the `TYLERTOO_STAGING_*`
    /// knobs: see `docs/OVERVIEW_TUNING.md` ("Remote staging (env)").
    const CLIENT_VAR_PREFIX: &str = "TYLERTOO_HTTP_";

    /// Parse `TYLERTOO_HTTP_*` variables out of `vars` into client options,
    /// sorted by key so the log line is stable. An unknown key, or a value
    /// `object_store` cannot parse, is skipped with a warning naming the
    /// variable (the same warn-and-ignore as the `TYLERTOO_STAGING_*` knobs).
    /// No environment access, so it is unit-testable;
    /// [`effective_client_overrides`] feeds it the process environment.
    fn client_overrides(
        vars: impl IntoIterator<Item = (String, String)>,
    ) -> (Vec<(ClientConfigKey, String)>, Vec<String>) {
        let mut out = Vec::new();
        let mut warnings = Vec::new();
        for (name, value) in vars {
            let Some(suffix) = name.strip_prefix(CLIENT_VAR_PREFIX) else {
                continue;
            };
            let Ok(key) = suffix.to_ascii_lowercase().parse::<ClientConfigKey>() else {
                warnings.push(format!(
                    "{name} names no object_store client option; ignoring it"
                ));
                continue;
            };
            // `object_store` defers parsing a value to the store build, and
            // its error there ("failed to parse \"x\" as Duration") names
            // neither key nor variable. Probe-build a client with this one
            // option so a bad value is caught here, by name. No I/O.
            let probe = HttpBuilder::new()
                .with_url("http://127.0.0.1")
                .with_config(key, &value)
                .build();
            match probe {
                Ok(_) => out.push((key, value)),
                // The parse error can echo the value, so a redacted key
                // gets neither.
                Err(_) if redacted(key) => warnings.push(format!("{name} is invalid; ignoring it")),
                Err(e) => warnings.push(format!("{name}={value} is invalid ({e}); ignoring it")),
            }
        }
        out.sort_by(|a, b| a.0.as_ref().cmp(b.0.as_ref()));
        (out, warnings)
    }

    /// Client options whose value is never logged: a proxy URL can carry
    /// `user:password@` credentials.
    fn redacted(key: ClientConfigKey) -> bool {
        matches!(key, ClientConfigKey::ProxyUrl)
    }

    /// The process-wide client overrides: resolved from the environment once,
    /// on the first store build, logging warnings plus one info line naming
    /// the overrides in force (so a #290 sweep run records what it tested).
    fn effective_client_overrides() -> &'static [(ClientConfigKey, String)] {
        #[expect(
            clippy::disallowed_types,
            reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
        )]
        static CELL: OnceLock<Vec<(ClientConfigKey, String)>> = OnceLock::new();
        CELL.get_or_init(|| {
            let vars = std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
            let (overrides, warnings) = client_overrides(vars);
            for w in &warnings {
                log::warn!("[remote] {w}");
            }
            if let Some(line) = overrides_log_line(&overrides) {
                log::info!("[remote] {line}");
            }
            overrides
        })
    }

    /// The info line naming the client overrides in force, or `None` when
    /// there are none. A [`redacted`] key's value is never shown. Split out so
    /// the wording is unit-testable without capturing a logger.
    fn overrides_log_line(overrides: &[(ClientConfigKey, String)]) -> Option<String> {
        if overrides.is_empty() {
            return None;
        }
        let list: Vec<String> = overrides
            .iter()
            .map(|(k, v)| {
                let v = if redacted(*k) { "<redacted>" } else { v };
                format!("{}={v}", k.as_ref())
            })
            .collect();
        Some(format!(
            "client options from {CLIENT_VAR_PREFIX}*: {}",
            list.join(", ")
        ))
    }

    /// Bridges the AWS SDK credential chain (profiles, SSO, IMDS, ...) into
    /// `object_store`'s credential provider, re-resolving on each request so
    /// expiring credentials refresh (the SDK chain caches internally).
    #[derive(Debug)]
    struct SdkCredentialBridge(aws_credential_types::provider::SharedCredentialsProvider);

    #[async_trait::async_trait]
    impl CredentialProvider for SdkCredentialBridge {
        type Credential = AwsCredential;

        async fn get_credential(&self) -> object_store::Result<Arc<AwsCredential>> {
            use aws_credential_types::provider::ProvideCredentials;
            let creds =
                self.0
                    .provide_credentials()
                    .await
                    .map_err(|e| object_store::Error::Generic {
                        store: "S3",
                        source: Box::new(e),
                    })?;
            Ok(Arc::new(AwsCredential {
                key_id: creds.access_key_id().to_string(),
                secret_key: creds.secret_access_key().to_string(),
                token: creds.session_token().map(str::to_string),
            }))
        }
    }

    /// Floor for the per-reader column-chunk fetch cache. The true working set
    /// is one row group's compressed column chunks (the arrow reader
    /// interleaves the columns of the row group it is decoding), so
    /// [`RemoteSource::open_builder`] raises the live cap to the largest row
    /// group's working set when that exceeds this floor — otherwise a row group
    /// bigger than the cache thrashes, re-fetching an evicted chunk on the next
    /// batch (issue #261). This constant is just the small-input floor.
    const CHUNK_CACHE_MAX_BYTES: u64 = 256 * 1024 * 1024;

    /// Staging (pass 0, #286/#287) concurrency ceiling: at most this many
    /// row-group spans are fetched — and thus resident — at once. The live
    /// concurrency is the lesser of this and [`STAGE_MEM_BUDGET`] divided by
    /// the largest span, so a file of huge row groups never holds more than
    /// the budget's worth of in-flight spans in memory. Default only:
    /// overridable via `TYLERTOO_STAGING_CONCURRENCY` ([`staging_tunables`],
    /// #290) until the sweep settles real, host-aware values.
    const STAGE_MAX_CONCURRENCY: usize = 8;

    /// Transient-memory budget bounding staging concurrency. One row group's
    /// working set is already the in-memory chunk-cache ceiling (#261 sizes
    /// the cache to the largest row group), so reuse that figure: staging
    /// never holds more than a budget's worth of in-flight spans at once.
    /// Default only: overridable via `TYLERTOO_STAGING_MEM_BUDGET_BYTES`
    /// ([`staging_tunables`], #290).
    const STAGE_MEM_BUDGET: u64 = CHUNK_CACHE_MAX_BYTES;

    /// Staging coalescing window (#290 dimension 2): adjacent selected row
    /// groups merge into one range request while the merged span stays at or
    /// under this many bytes; a pruned row group always splits a merge. `0`
    /// (the default) keeps one request per row group, as #289 shipped.
    /// Overridable via `TYLERTOO_STAGING_COALESCE_BYTES`
    /// ([`staging_tunables`]).
    const STAGE_COALESCE_WINDOW: u64 = 0;

    /// Pass-0 staging tunables (#290): the staging constants above, each
    /// overridable through a `TYLERTOO_STAGING_*` environment variable so the
    /// #290 parameter sweep can vary them per run without rebuilding.
    /// Experimental sweep knobs, not stable API — see
    /// `docs/OVERVIEW_TUNING.md` ("Remote staging (env)").
    ///
    /// The knobs change I/O *scheduling* only (how many range requests are in
    /// flight at once, and where one request ends and the next begins): the
    /// same bytes are staged into the
    /// same spill slots keyed by chunk offset, and both passes consume them
    /// through the reader's unchanged path, so output stays byte-identical
    /// whatever the values.
    mod staging_tunables {
        use super::{STAGE_COALESCE_WINDOW, STAGE_MAX_CONCURRENCY, STAGE_MEM_BUDGET};

        /// Override for [`STAGE_MAX_CONCURRENCY`]: ceiling on in-flight
        /// row-group spans.
        pub(super) const CONCURRENCY_VAR: &str = "TYLERTOO_STAGING_CONCURRENCY";
        /// Override for [`STAGE_MEM_BUDGET`]: transient-memory budget, in
        /// bytes (same unit as `TYLERTOO_AUTO_MEM_LIMIT_BYTES`).
        pub(super) const MEM_BUDGET_VAR: &str = "TYLERTOO_STAGING_MEM_BUDGET_BYTES";
        /// Override for [`STAGE_COALESCE_WINDOW`], in bytes.
        pub(super) const COALESCE_VAR: &str = "TYLERTOO_STAGING_COALESCE_BYTES";

        /// Hard ceiling on the concurrency override: each in-flight span is a
        /// task holding a whole row group's bytes, so an absurd value is a
        /// memory/socket bomb, not a sweep point.
        pub(super) const CONCURRENCY_MAX: usize = 128;
        /// Floor for the budget override: below ~1 MiB the budget-derived
        /// concurrency is pinned to 1 anyway, and a 0 budget is pathological.
        const MEM_BUDGET_MIN: u64 = 1024 * 1024;
        /// Ceiling on the window override: a merged span is held whole in
        /// memory while it downloads, so past 1 GiB the window is a memory
        /// bomb rather than a sweep point.
        pub(super) const COALESCE_MAX: u64 = 1024 * 1024 * 1024;

        /// The resolved staging knobs. Defaults are the constants that were
        /// hardcoded before #290's minimum landed.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) struct StagingTunables {
            /// Ceiling on in-flight row-group spans ([`CONCURRENCY_VAR`]).
            pub(super) max_concurrency: usize,
            /// Transient-memory budget in bytes ([`MEM_BUDGET_VAR`]); live
            /// concurrency is `min(max_concurrency, mem_budget / largest span)`.
            pub(super) mem_budget: u64,
            /// Largest merged span, in bytes ([`COALESCE_VAR`]); `0` = one
            /// request per row group.
            pub(super) coalesce_window: u64,
            /// Was any variable set? Only [`effective`] knows (the
            /// resolver's `lookup` seam cannot distinguish "unset" from
            /// "set to the default"), so [`resolve`] always leaves this
            /// false; it exists to decide the log level of the effective
            /// and live-concurrency lines.
            pub(super) overridden: bool,
        }

        impl Default for StagingTunables {
            fn default() -> Self {
                Self {
                    max_concurrency: STAGE_MAX_CONCURRENCY,
                    mem_budget: STAGE_MEM_BUDGET,
                    coalesce_window: STAGE_COALESCE_WINDOW,
                    overridden: false,
                }
            }
        }

        /// Resolve the tunables through `lookup` (a `&str -> Option<String>`
        /// seam, so unit tests never touch the process-global environment).
        ///
        /// Never panics and never yields a pathological value: an unset
        /// variable keeps its default; an unparsable value falls back to the
        /// default; a parsable but out-of-range value is clamped to the
        /// nearest bound. Each bad value produces exactly one warning naming
        /// its variable (#429 discipline). Warnings are *returned*, not
        /// logged, so the behavior is unit-testable; [`effective`] logs them.
        pub(super) fn resolve(
            lookup: impl Fn(&str) -> Option<String>,
        ) -> (StagingTunables, Vec<String>) {
            let mut warnings = Vec::new();
            let max_concurrency = resolve_one(
                &lookup,
                CONCURRENCY_VAR,
                STAGE_MAX_CONCURRENCY,
                1,
                CONCURRENCY_MAX,
                &mut warnings,
            );
            let mem_budget = resolve_one(
                &lookup,
                MEM_BUDGET_VAR,
                STAGE_MEM_BUDGET,
                MEM_BUDGET_MIN,
                u64::MAX,
                &mut warnings,
            );
            let mut coalesce_window = resolve_one(
                &lookup,
                COALESCE_VAR,
                STAGE_COALESCE_WINDOW,
                0,
                COALESCE_MAX,
                &mut warnings,
            );
            // A merged span is held whole while it downloads, so a window
            // over the budget would overshoot it even at concurrency 1.
            if coalesce_window > mem_budget {
                warnings.push(format!(
                    "{COALESCE_VAR}={coalesce_window} exceeds the staging memory \
                     budget ({mem_budget} B, {MEM_BUDGET_VAR}); clamping the \
                     window to the budget"
                ));
                coalesce_window = mem_budget;
            }
            (
                StagingTunables {
                    max_concurrency,
                    mem_budget,
                    coalesce_window,
                    overridden: false,
                },
                warnings,
            )
        }

        /// One knob: unset → `default`; unparsable → `default` + warning;
        /// parsed but outside `min..=max` → clamped to the violated bound +
        /// warning. Whitespace around the value is tolerated (same as the
        /// `TYLERTOO_AUTO_MEM_LIMIT_BYTES` resolver).
        fn resolve_one<T>(
            lookup: impl Fn(&str) -> Option<String>,
            var: &str,
            default: T,
            min: T,
            max: T,
            warnings: &mut Vec<String>,
        ) -> T
        where
            T: std::str::FromStr + Ord + Copy + std::fmt::Display,
        {
            let Some(raw) = lookup(var) else {
                return default;
            };
            match raw.trim().parse::<T>() {
                Ok(v) if v < min => {
                    warnings.push(format!(
                        "{var}={raw} is below the minimum {min}; clamping to {min}"
                    ));
                    min
                }
                Ok(v) if v > max => {
                    warnings.push(format!(
                        "{var}={raw} is above the maximum {max}; clamping to {max}"
                    ));
                    max
                }
                Ok(v) => v,
                Err(_) => {
                    // A plain non-negative integer that merely overflows `T`
                    // is out of range, not malformed, so clamp it like any
                    // other out-of-range value. Falling back to the default
                    // here would contradict the documented behaviour and
                    // report "is not a non-negative integer" about a string
                    // that plainly is one.
                    let digits = raw.trim();
                    let digits = digits.strip_prefix('+').unwrap_or(digits);
                    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                        warnings.push(format!(
                            "{var}={raw} is above the maximum {max}; clamping to {max}"
                        ));
                        max
                    } else {
                        warnings.push(format!(
                            "{var}={raw:?} is not a non-negative integer; \
                             using the default {default}"
                        ));
                        default
                    }
                }
            }
        }

        /// The process-wide effective tunables: resolved from the real
        /// environment once, on first use (i.e. at staging start), logging
        /// any fallback/clamp warnings plus one line with the effective
        /// values — info when an override is set (so a #290 sweep run can
        /// confirm what it tested), debug otherwise.
        pub(super) fn effective() -> &'static StagingTunables {
            #[expect(
                clippy::disallowed_types,
                reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
            )]
            use std::sync::OnceLock;
            #[expect(
                clippy::disallowed_types,
                reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
            )]
            static CELL: OnceLock<StagingTunables> = OnceLock::new();
            CELL.get_or_init(|| {
                let lookup = |name: &str| std::env::var(name).ok();
                let overridden = [CONCURRENCY_VAR, MEM_BUDGET_VAR, COALESCE_VAR]
                    .iter()
                    .any(|v| lookup(v).is_some());
                let (mut tunables, warnings) = resolve(lookup);
                tunables.overridden = overridden;
                for w in &warnings {
                    log::warn!("[stage] {w}");
                }
                let line = format!(
                    "[stage] staging tunables: concurrency ceiling={} \
                     ({CONCURRENCY_VAR}), mem budget={} B ({MEM_BUDGET_VAR}), \
                     coalesce window={} B ({COALESCE_VAR})",
                    tunables.max_concurrency, tunables.mem_budget, tunables.coalesce_window
                );
                if overridden {
                    log::info!("{line}");
                } else {
                    log::debug!("{line}");
                }
                tunables
            })
        }

        /// Log this operation's *live* in-flight concurrency, at info
        /// when an override is set.
        ///
        /// [`effective`]'s line reports the concurrency *ceiling*, but the
        /// live value is `min(ceiling, budget / largest span)`. A budget that
        /// caps a requested ceiling would otherwise leave a #290 sweep
        /// recording a concurrency it never actually ran — the 8 and 16
        /// points of a 1/2/4/8/16 sweep produce identical runs on an input
        /// whose row groups are large enough, and only the ceiling appears in
        /// the log. Each staging operation reports its own value, including
        /// later operations in the same process.
        pub(super) fn log_live_concurrency(t: &StagingTunables, concurrency: usize, max_span: u64) {
            if !t.overridden {
                return;
            }
            if concurrency < t.max_concurrency {
                log::info!(
                    "[stage] live concurrency {concurrency}: the {} B budget \
                     over the largest {max_span} B span caps the {} ceiling",
                    t.mem_budget,
                    t.max_concurrency
                );
            } else {
                log::info!(
                    "[stage] live concurrency {concurrency}, at the {} ceiling",
                    t.max_concurrency
                );
            }
        }
    }

    /// One staging request: a contiguous byte span covering one selected row
    /// group, or several adjacent ones once [`coalesce_spans`] merges them,
    /// plus the `(chunk_start, len)` of every column chunk inside it — keyed
    /// exactly how the L2 spill and [`RemoteReader::chunk_data`] look chunks
    /// up (`chunk.start`), so a staged slice is a drop-in L2 hit.
    struct StagedSpan {
        span: Range<u64>,
        chunks: Vec<(u64, usize)>,
    }

    /// Merge adjacent row-group spans into fewer, larger requests (#290
    /// dimension 2). Neighbours in file order merge while the merged span
    /// stays within `window` bytes and no `blocked` (unselected) row group
    /// lies between them; unowned bytes between two selected spans count
    /// toward the window. `window == 0` returns one span per row group, and a
    /// row group larger than the window stays a request of its own.
    ///
    /// Only request boundaries move: every chunk keeps its own spill slot, so
    /// the staged bytes, and the output, do not change.
    fn coalesce_spans(
        mut plan: Vec<StagedSpan>,
        blocked: &[Range<u64>],
        window: u64,
    ) -> Vec<StagedSpan> {
        plan.sort_by_key(|s| s.span.start);
        // Blocked spans by start, with a running max of their ends, so "does
        // any blocked span overlap this gap" is one binary search rather than
        // a scan per pair (a part can have tens of thousands of row groups).
        let mut blocked = blocked.to_vec();
        blocked.sort_by_key(|b| b.start);
        let max_end: Vec<u64> = blocked
            .iter()
            .scan(0u64, |m, b| {
                *m = (*m).max(b.end);
                Some(*m)
            })
            .collect();
        let gap_is_clear = |gap: &Range<u64>| {
            let n = blocked.partition_point(|b| b.start < gap.end);
            n == 0 || max_end[n - 1] <= gap.start
        };
        let mut out: Vec<StagedSpan> = Vec::with_capacity(plan.len());
        for next in plan {
            if let Some(cur) = out.last_mut() {
                let gap = cur.span.end..next.span.start;
                let fits = next.span.end.saturating_sub(cur.span.start) <= window;
                if fits && gap.start <= gap.end && gap_is_clear(&gap) {
                    cur.span.end = next.span.end;
                    cur.chunks.extend(next.chunks);
                    continue;
                }
            }
            out.push(next);
        }
        out
    }

    /// On-disk overflow for fetched column chunks (issue #219).
    ///
    /// The in-memory [`ChunkCache`] holds only one row group's working set, so
    /// across the streaming pipeline's multiple passes (assign, coarse levels,
    /// finest streamed last) a chunk evicted from memory is otherwise
    /// re-fetched over the network — paying remote bandwidth 2–3× for the bulk
    /// of the file (measured 3.0× on fieldmaps-adm4). Spilling every fetched
    /// chunk to a local temp file and draining re-reads from disk bounds remote
    /// traffic to ≈1× the object, regardless of pass or level count.
    ///
    /// The temp file is anonymous ([`tempfile::tempfile`]): it is unlinked on
    /// creation, so it never appears in the filesystem and the OS reclaims its
    /// space when the last handle drops. Its directory follows `TMPDIR`; point
    /// that at real disk if the default temp dir is a small tmpfs.
    ///
    /// Best-effort: if the temp file cannot be created or an I/O op fails, the
    /// spill disables itself (logging once) and the reader falls back to
    /// network re-fetch — correctness is unaffected, only the re-fetch cost
    /// returns. Access is serialized by the enclosing `Mutex`; the pipeline's
    /// passes read the input sequentially, so lock contention is negligible.
    #[derive(Debug, Default)]
    struct DiskSpill {
        /// Lazily created on the first spilled chunk; `None` until then, or
        /// left `None` after a spill error disables the cache.
        file: Option<File>,
        /// `chunk.start` → (offset within the spill file, byte length).
        index: std::collections::HashMap<u64, (u64, usize)>,
        /// Append cursor: total bytes written to the spill file so far.
        write_offset: u64,
        /// Set after a create/read/write error so we stop touching disk.
        disabled: bool,
        /// Directory for the (anonymous) spill file — `--spill-dir`, issue
        /// #272. `None` follows the process temp dir (`$TMPDIR`). Honored
        /// at file creation, i.e. on the first spilled chunk.
        dir: Option<PathBuf>,
    }

    /// #273: the warning to emit when `dir` (the resolved spill directory)
    /// lives on a RAM-backed filesystem, or `None` when it is real disk or
    /// detection is unavailable (best-effort). Spilling to tmpfs/ramfs trades
    /// network bytes for memory pressure, defeating the spill and risking OOM.
    /// Split out so the wording is unit-testable without capturing a logger.
    fn ram_backed_spill_warning(dir: &Path) -> Option<String> {
        (crate::fs_probe::is_ram_backed(dir) == Some(true)).then(|| {
            format!(
                "input spill directory {} is on a RAM-backed filesystem \
                 (tmpfs/ramfs); spilling there consumes memory instead of disk \
                 and can OOM on large inputs — point --spill-dir (or $TMPDIR) at \
                 a real-disk location",
                dir.display()
            )
        })
    }

    impl DiskSpill {
        /// Serve a previously spilled chunk, if present. `None` means "not
        /// spilled — fetch it over the network"; a read error disables the
        /// spill and also returns `None` so the caller falls back to fetch.
        fn get(&mut self, chunk_start: u64) -> Option<Bytes> {
            if self.disabled {
                return None;
            }
            let (offset, len) = *self.index.get(&chunk_start)?;
            let file = self.file.as_mut()?;
            let mut buf = vec![0u8; len];
            match file
                .seek(SeekFrom::Start(offset))
                .and_then(|_| file.read_exact(&mut buf))
            {
                Ok(()) => Some(Bytes::from(buf)),
                Err(e) => {
                    log::warn!("input spill read failed ({e}); falling back to network re-fetch");
                    self.disabled = true;
                    None
                }
            }
        }

        /// Record a freshly fetched chunk on disk for later passes. No-op if
        /// the spill is disabled or already holds this chunk; a create/write
        /// error disables the spill.
        fn put(&mut self, chunk_start: u64, data: &Bytes) {
            if self.disabled || self.index.contains_key(&chunk_start) {
                return;
            }
            if self.file.is_none() {
                // #273: warn once (the spill file is created exactly once) when
                // the resolved directory is RAM-backed. `tempfile()` follows
                // `std::env::temp_dir()` ($TMPDIR or /tmp) when no dir is set.
                let resolved = self.dir.clone().unwrap_or_else(std::env::temp_dir);
                if let Some(msg) = ram_backed_spill_warning(&resolved) {
                    log::warn!("{msg}");
                }
                let created = match self.dir.as_deref() {
                    Some(d) => tempfile::tempfile_in(d),
                    None => tempfile::tempfile(),
                };
                match created {
                    Ok(f) => self.file = Some(f),
                    Err(e) => {
                        let dir = self
                            .dir
                            .clone()
                            .unwrap_or_else(std::env::temp_dir)
                            .display()
                            .to_string();
                        log::warn!(
                            "could not create input spill file in {dir} ({e}); remote \
                             re-reads will re-fetch over the network"
                        );
                        self.disabled = true;
                        return;
                    }
                }
            }
            let offset = self.write_offset;
            let file = self.file.as_mut().expect("spill file present");
            if let Err(e) = file
                .seek(SeekFrom::Start(offset))
                .and_then(|_| file.write_all(data))
            {
                log::warn!("input spill write failed ({e}); falling back to network re-fetch");
                self.disabled = true;
                return;
            }
            self.write_offset += data.len() as u64;
            self.index.insert(chunk_start, (offset, data.len()));
        }
    }

    /// Per-reader cache of whole column chunks, insertion-ordered for
    /// eviction. This is the "buffered range-fetch adapter": the page reader
    /// asks for a column chunk's bytes in many small pieces (a thrift page
    /// header via `get_read`, then each page via `get_bytes`); fetching the
    /// whole chunk on first touch turns that into ONE range request per
    /// selected column chunk. Chunks of bbox-pruned row groups are never
    /// touched, so they are still never fetched.
    ///
    /// `cap` is the eviction budget. It starts at the [`CHUNK_CACHE_MAX_BYTES`]
    /// floor and is raised at open time to the largest row group's working set
    /// ([`RemoteSource::open_builder`]) so that a row group whose column chunks
    /// exceed the floor is never evicted mid-read — the fix for issue #261,
    /// where a >256 MiB geometry chunk was evicted on insert and re-fetched
    /// on every page read (measured 96× re-fetch on a vertex-heavy input).
    #[derive(Debug)]
    struct ChunkCache {
        entries: std::collections::HashMap<u64, (Range<u64>, Bytes)>,
        order: std::collections::VecDeque<u64>,
        total: u64,
        cap: u64,
    }

    impl ChunkCache {
        fn new(cap: u64) -> Self {
            ChunkCache {
                entries: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
                total: 0,
                cap,
            }
        }
    }

    /// Range-request reader over one remote object. Cloneable; all clones
    /// share the source's counters (the chunk cache too — it lives per
    /// source, so multi-pass pipelines could reuse it, though in practice
    /// eviction keeps it near one row group).
    #[derive(Debug, Clone)]
    pub struct RemoteReader {
        store: Arc<dyn ObjectStore>,
        location: ObjectPath,
        object_meta: tylertoo_io::RemoteObjectMeta,
        shared: Arc<SharedState>,
        cache: Arc<Mutex<ChunkCache>>,
        spill: Arc<Mutex<DiskSpill>>,
    }

    impl RemoteReader {
        /// Total object size (the [`tylertoo_io::Length`] answer).
        pub(crate) fn object_size(&self) -> u64 {
            self.object_meta.size
        }

        /// One counted range GET.
        fn fetch(&self, range: Range<u64>) -> Result<Bytes, ParquetError> {
            let bytes = runtime()
                .block_on(self.object_meta.get_range(
                    self.store.as_ref(),
                    &self.location,
                    range.clone(),
                ))
                .map_err(|e| ParquetError::External(Box::new(e)))?;
            self.shared.requests.fetch_add(1, Ordering::Relaxed);
            self.shared
                .bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            self.shared
                .ranges
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(range);
            Ok(bytes)
        }

        /// The column chunk containing `start..end`, if the footer is parsed
        /// and the range falls entirely inside one chunk.
        fn chunk_containing(&self, start: u64, end: u64) -> Option<Range<u64>> {
            let chunks = self.shared.chunk_ranges.get()?;
            let idx = chunks.partition_point(|r| r.start <= start);
            let chunk = chunks.get(idx.checked_sub(1)?)?;
            (start >= chunk.start && end <= chunk.end).then(|| chunk.clone())
        }

        /// Bytes of a whole column chunk, served from the cheapest tier that
        /// holds it: the in-memory cache (L1), the local disk spill (L2, #219),
        /// or a single network range request (L3). A byte therefore crosses the
        /// network at most once across the pipeline's passes.
        fn chunk_data(&self, chunk: &Range<u64>) -> Result<Bytes, ParquetError> {
            // L1: in-memory cache (hot, one row group's working set).
            {
                let cache = self
                    .cache
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some((_, data)) = cache.entries.get(&chunk.start) {
                    return Ok(data.clone());
                }
            }
            // L2: local disk spill. A hit avoids re-fetching over the network on
            // a later pass; re-warm L1 so same-pass touches stay in memory.
            if let Some(data) = self
                .spill
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(chunk.start)
            {
                self.cache_insert(chunk, &data);
                return Ok(data);
            }
            // L3: network. Fetch once, then spill the bytes for later passes.
            let data = self.fetch(chunk.clone())?;
            // #430: a store that returns fewer bytes than the chunk declares
            // (truncated download) is an error here, before anything is
            // cached or sliced from it.
            if data.len() as u64 != chunk.end - chunk.start {
                return Err(self.short_chunk_error(chunk, data.len()));
            }
            self.spill
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .put(chunk.start, &data);
            self.cache_insert(chunk, &data);
            Ok(data)
        }

        /// Insert a chunk into the in-memory L1 cache, evicting in insertion
        /// order until the working-set budget (`cap`, sized to the largest row
        /// group in [`RemoteSource::open_builder`]) is respected.
        fn cache_insert(&self, chunk: &Range<u64>, data: &Bytes) {
            let mut cache = self
                .cache
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !cache.entries.contains_key(&chunk.start) {
                cache.total += data.len() as u64;
                cache
                    .entries
                    .insert(chunk.start, (chunk.clone(), data.clone()));
                cache.order.push_back(chunk.start);
                while cache.total > cache.cap {
                    let Some(oldest) = cache.order.pop_front() else {
                        break;
                    };
                    if let Some((_, evicted)) = cache.entries.remove(&oldest) {
                        cache.total -= evicted.len() as u64;
                    }
                }
            }
        }

        /// Exact-range read for [`tylertoo_io::ChunkReader::get_bytes`].
        pub(crate) fn get_bytes_range(
            &self,
            start: u64,
            length: usize,
        ) -> Result<Bytes, ParquetError> {
            let end = start
                .checked_add(length as u64)
                .filter(|end| *end <= self.object_meta.size)
                .ok_or_else(|| {
                    ParquetError::EOF(format!(
                        "range {start}..{} beyond object size {} for {}",
                        start as u128 + length as u128,
                        self.object_meta.size,
                        self.location
                    ))
                })?;
            if length == 0 {
                return Ok(Bytes::new());
            }
            // Page reads land inside a column chunk: serve them from the
            // whole-chunk buffer (one request per chunk). Everything else
            // (footer tail, metadata) is fetched exactly.
            if let Some(chunk) = self.chunk_containing(start, end) {
                let data = self.chunk_data(&chunk)?;
                // Checked (#430): the store may have returned fewer bytes
                // than the chunk declares (truncated download); slicing
                // past `data.len()` would panic.
                let slice = usize::try_from(start - chunk.start)
                    .ok()
                    .and_then(|off| off.checked_add(length).map(|e| off..e))
                    .filter(|r| r.end <= data.len())
                    .ok_or_else(|| self.short_chunk_error(&chunk, data.len()))?;
                return Ok(data.slice(slice));
            }
            self.fetch(start..end)
        }

        /// The error for a column chunk the store returned short (#430).
        fn short_chunk_error(&self, chunk: &Range<u64>, got: usize) -> ParquetError {
            ParquetError::EOF(format!(
                "column chunk {chunk:?} of {} returned {got} bytes, expected {} \
                 (truncated download?)",
                self.location,
                chunk.end - chunk.start
            ))
        }

        /// Chunked sequential reader for
        /// [`tylertoo_io::ChunkReader::get_read`] (not used by the
        /// arrow reader path, provided for trait completeness).
        pub(crate) fn sequential_reader(&self, start: u64) -> SequentialRemoteRead {
            SequentialRemoteRead {
                reader: self.clone(),
                pos: start,
                buf: Bytes::new(),
                buf_offset: 0,
            }
        }
    }

    /// `Read` adapter fetching forward in [`SEQUENTIAL_CHUNK`] steps, each
    /// step clamped to the column chunk containing the read position.
    pub(crate) struct SequentialRemoteRead {
        reader: RemoteReader,
        pos: u64,
        buf: Bytes,
        buf_offset: usize,
    }

    impl std::io::Read for SequentialRemoteRead {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.buf_offset >= self.buf.len() {
                let remaining = self.reader.object_meta.size.saturating_sub(self.pos);
                if remaining == 0 {
                    return Ok(0);
                }
                // Inside a column chunk (the page-header read path): serve
                // the rest of the chunk from the whole-chunk buffer.
                if let Some(chunk) = self.reader.chunk_containing(self.pos, self.pos + 1) {
                    let data = self
                        .reader
                        .chunk_data(&chunk)
                        .map_err(std::io::Error::other)?;
                    // Checked (#430): a short chunk must not slice past
                    // the bytes the store actually returned.
                    let offset = usize::try_from(self.pos - chunk.start)
                        .ok()
                        .filter(|off| *off <= data.len())
                        .ok_or_else(|| {
                            std::io::Error::other(self.reader.short_chunk_error(&chunk, data.len()))
                        })?;
                    self.buf = data.slice(offset..);
                    self.buf_offset = 0;
                    self.pos = chunk.end;
                } else {
                    let want_end = self.pos + remaining.min(SEQUENTIAL_CHUNK);
                    // Never read across a column-chunk boundary: bytes past
                    // it may belong to a bbox-pruned row group that must not
                    // be downloaded.
                    let end = self.reader.shared.clamp_to_chunk(self.pos, want_end);
                    self.buf = self
                        .reader
                        .fetch(self.pos..end)
                        .map_err(std::io::Error::other)?;
                    self.buf_offset = 0;
                    self.pos = end;
                }
            }
            let n = out.len().min(self.buf.len() - self.buf_offset);
            out[..n].copy_from_slice(&self.buf[self.buf_offset..self.buf_offset + n]);
            self.buf_offset += n;
            Ok(n)
        }
    }

    #[cfg(test)]
    mod staging_tunables_tests {
        //! #290 (minimum scope): the `TYLERTOO_STAGING_*` resolver. All tests
        //! inject the lookup closure — the process environment is never
        //! touched, so these are safe under parallel test execution.

        use super::staging_tunables::{
            resolve, StagingTunables, COALESCE_MAX, COALESCE_VAR, CONCURRENCY_MAX, CONCURRENCY_VAR,
            MEM_BUDGET_VAR,
        };
        use super::{STAGE_MAX_CONCURRENCY, STAGE_MEM_BUDGET};

        /// Lookup over a static `(name, value)` table.
        fn env(vars: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
            move |name| {
                vars.iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| v.to_string())
            }
        }

        #[test]
        fn unset_vars_keep_the_hardcoded_defaults_with_no_warnings() {
            let (t, warnings) = resolve(env(&[]));
            assert_eq!(t, StagingTunables::default());
            assert_eq!(t.max_concurrency, STAGE_MAX_CONCURRENCY);
            assert_eq!(t.mem_budget, STAGE_MEM_BUDGET);
            assert!(warnings.is_empty(), "defaults must not warn: {warnings:?}");
        }

        #[test]
        fn valid_overrides_are_honored() {
            let (t, warnings) = resolve(env(&[
                (CONCURRENCY_VAR, "16"),
                (MEM_BUDGET_VAR, "67108864"),
            ]));
            assert_eq!(t.max_concurrency, 16);
            assert_eq!(t.mem_budget, 64 * 1024 * 1024);
            assert!(
                warnings.is_empty(),
                "valid values must not warn: {warnings:?}"
            );
        }

        #[test]
        fn surrounding_whitespace_is_tolerated() {
            let (t, warnings) = resolve(env(&[(CONCURRENCY_VAR, " 4\n")]));
            assert_eq!(t.max_concurrency, 4);
            assert!(warnings.is_empty(), "{warnings:?}");
        }

        #[test]
        fn unparsable_values_warn_once_naming_the_var_and_keep_the_default() {
            for bad in ["", "eight", "-3", "1.5", "8MB"] {
                let (t, warnings) = resolve(env(&[]).into_overlay(CONCURRENCY_VAR, bad));
                assert_eq!(
                    t.max_concurrency, STAGE_MAX_CONCURRENCY,
                    "bad value {bad:?} must fall back to the default"
                );
                assert_eq!(warnings.len(), 1, "exactly one warning for {bad:?}");
                assert!(
                    warnings[0].contains(CONCURRENCY_VAR),
                    "warning must name the variable: {}",
                    warnings[0]
                );
            }
        }

        #[test]
        fn values_that_overflow_the_target_type_clamp_to_the_maximum() {
            // A string of digits too large for usize/u64 is out of range,
            // not malformed: the documented behaviour is "clamps to the
            // bound", and reporting "is not a non-negative integer" about
            // "99999999999999999999" would be plainly wrong.
            let (t, warnings) = resolve(env(&[(CONCURRENCY_VAR, "99999999999999999999")]));
            assert_eq!(t.max_concurrency, CONCURRENCY_MAX);
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(
                warnings[0].contains("above the maximum"),
                "overflow must read as out-of-range, not unparsable: {}",
                warnings[0]
            );

            let (t, warnings) = resolve(env(&[(MEM_BUDGET_VAR, "10000000000000000000000")]));
            assert_eq!(t.mem_budget, u64::MAX);
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains("above the maximum"), "{}", warnings[0]);
        }

        #[test]
        fn a_negative_or_malformed_value_still_falls_back_to_the_default() {
            // The clamp above must not swallow genuinely malformed input:
            // "-3" is not a non-negative integer, so it keeps the default
            // rather than clamping to the minimum.
            for bad in ["-3", "+", "1e3", "0x10"] {
                let (t, warnings) = resolve(env(&[]).into_overlay(CONCURRENCY_VAR, bad));
                assert_eq!(
                    t.max_concurrency, STAGE_MAX_CONCURRENCY,
                    "{bad:?} must fall back to the default"
                );
                assert!(
                    warnings[0].contains("not a non-negative integer"),
                    "{bad:?}: {}",
                    warnings[0]
                );
            }
        }

        /// Overlay helper: `base` plus one `(name, value)` pair.
        trait Overlay: Fn(&str) -> Option<String> + Sized {
            fn into_overlay(
                self,
                name: &'static str,
                value: &'static str,
            ) -> impl Fn(&str) -> Option<String> {
                move |n: &str| {
                    if n == name {
                        Some(value.to_string())
                    } else {
                        self(n)
                    }
                }
            }
        }
        impl<F: Fn(&str) -> Option<String>> Overlay for F {}

        #[test]
        fn zero_concurrency_is_clamped_to_one_with_a_warning() {
            let (t, warnings) = resolve(env(&[(CONCURRENCY_VAR, "0")]));
            assert_eq!(
                t.max_concurrency, 1,
                "0 concurrency is pathological; clamp to 1"
            );
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains(CONCURRENCY_VAR), "{}", warnings[0]);
        }

        #[test]
        fn absurd_concurrency_is_clamped_to_the_ceiling_with_a_warning() {
            let (t, warnings) = resolve(env(&[(CONCURRENCY_VAR, "100000")]));
            assert_eq!(t.max_concurrency, 128);
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains(CONCURRENCY_VAR), "{}", warnings[0]);
        }

        #[test]
        fn tiny_mem_budget_is_clamped_to_the_floor_with_a_warning() {
            for bad in ["0", "1", "1048575"] {
                let (t, warnings) = resolve(env(&[]).into_overlay(MEM_BUDGET_VAR, bad));
                assert_eq!(
                    t.mem_budget,
                    1024 * 1024,
                    "budget {bad:?} must clamp to the 1 MiB floor"
                );
                assert_eq!(warnings.len(), 1, "exactly one warning for {bad:?}");
                assert!(warnings[0].contains(MEM_BUDGET_VAR), "{}", warnings[0]);
            }
        }

        #[test]
        fn unparsable_mem_budget_warns_and_keeps_the_default() {
            let (t, warnings) = resolve(env(&[(MEM_BUDGET_VAR, "lots")]));
            assert_eq!(t.mem_budget, STAGE_MEM_BUDGET);
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains(MEM_BUDGET_VAR), "{}", warnings[0]);
        }

        #[test]
        fn one_bad_value_does_not_poison_the_other_knob() {
            let (t, warnings) = resolve(env(&[
                (CONCURRENCY_VAR, "junk"),
                (MEM_BUDGET_VAR, "2097152"),
            ]));
            assert_eq!(t.max_concurrency, STAGE_MAX_CONCURRENCY);
            assert_eq!(
                t.mem_budget,
                2 * 1024 * 1024,
                "valid budget must survive a bad concurrency"
            );
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains(CONCURRENCY_VAR), "{}", warnings[0]);
        }

        #[test]
        fn coalesce_window_defaults_to_off() {
            let (t, warnings) = resolve(env(&[]));
            assert_eq!(t.coalesce_window, 0, "default is one request per row group");
            assert!(warnings.is_empty(), "{warnings:?}");
        }

        #[test]
        fn coalesce_window_override_is_honored_and_clamped() {
            let (t, warnings) = resolve(env(&[(COALESCE_VAR, "67108864")]));
            assert_eq!(t.coalesce_window, 64 * 1024 * 1024);
            assert!(warnings.is_empty(), "{warnings:?}");

            // Budget raised past the ceiling so only the ceiling clamps.
            let (t, warnings) = resolve(env(&[
                (COALESCE_VAR, "99999999999"),
                (MEM_BUDGET_VAR, "4294967296"),
            ]));
            assert_eq!(t.coalesce_window, COALESCE_MAX);
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains(COALESCE_VAR), "{}", warnings[0]);

            let (t, warnings) = resolve(env(&[(COALESCE_VAR, "big")]));
            assert_eq!(t.coalesce_window, 0);
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains(COALESCE_VAR), "{}", warnings[0]);
        }

        #[test]
        fn coalesce_window_is_clamped_to_the_memory_budget() {
            // 512 MiB window over the default 256 MiB budget: a merged span is
            // held whole, so it must not outgrow the budget.
            let (t, warnings) = resolve(env(&[(COALESCE_VAR, "536870912")]));
            assert_eq!(t.coalesce_window, STAGE_MEM_BUDGET);
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(warnings[0].contains(COALESCE_VAR), "{}", warnings[0]);
            assert!(warnings[0].contains(MEM_BUDGET_VAR), "{}", warnings[0]);

            let (t, warnings) = resolve(env(&[
                (COALESCE_VAR, "8388608"),
                (MEM_BUDGET_VAR, "4194304"),
            ]));
            assert_eq!(t.coalesce_window, 4 * 1024 * 1024);
            assert_eq!(warnings.len(), 1, "{warnings:?}");
        }

        #[test]
        fn a_window_equal_to_the_memory_budget_is_not_clamped() {
            let (t, warnings) = resolve(env(&[
                (COALESCE_VAR, "4194304"),
                (MEM_BUDGET_VAR, "4194304"),
            ]));
            assert_eq!(t.coalesce_window, 4 * 1024 * 1024);
            assert!(warnings.is_empty(), "{warnings:?}");
        }
    }

    #[cfg(test)]
    mod coalesce_tests {
        //! #290 dimension 2: merge adjacent selected row-group spans into one
        //! request up to a window, never across a pruned row group.

        use super::staging_tunables::StagingTunables;
        use super::{coalesce_spans, StagedSpan};
        use crate::input::InputSource;
        use std::ops::Range;

        /// A span over `range` whose chunks are two halves of it.
        fn span(range: Range<u64>) -> StagedSpan {
            let mid = range.start + (range.end - range.start) / 2;
            StagedSpan {
                chunks: vec![
                    (range.start, (mid - range.start) as usize),
                    (mid, (range.end - mid) as usize),
                ],
                span: range,
            }
        }

        fn spans(out: &[StagedSpan]) -> Vec<Range<u64>> {
            out.iter().map(|s| s.span.clone()).collect()
        }

        #[test]
        fn window_zero_keeps_one_request_per_row_group() {
            let plan = vec![span(4..104), span(104..204), span(204..304)];
            let out = coalesce_spans(plan, &[], 0);
            assert_eq!(spans(&out), vec![4..104, 104..204, 204..304]);
        }

        #[test]
        fn adjacent_spans_merge_up_to_the_window() {
            let plan = vec![
                span(4..104),
                span(104..204),
                span(204..304),
                span(304..404),
                span(404..504),
            ];
            // 200 B window: pairs merge, the fifth stays alone.
            let out = coalesce_spans(plan, &[], 200);
            assert_eq!(spans(&out), vec![4..204, 204..404, 404..504]);
            // Every chunk survives the merge, in file order.
            assert_eq!(out[0].chunks.len(), 4);
            assert_eq!(out[0].chunks[0], (4, 50));
            assert_eq!(out[0].chunks[3], (154, 50));
        }

        #[test]
        fn a_large_window_merges_everything_contiguous() {
            let plan = vec![span(4..104), span(104..204), span(204..304)];
            let out = coalesce_spans(plan, &[], u64::MAX);
            assert_eq!(spans(&out), vec![4..304]);
            assert_eq!(out[0].chunks.len(), 6);
        }

        #[test]
        fn never_bridges_a_pruned_row_group() {
            // Row group 104..204 was pruned: its bytes must never be fetched
            // (#219/#210), however large the window.
            let plan = vec![span(4..104), span(204..304), span(304..404)];
            let pruned = 104..204;
            let out = coalesce_spans(plan, std::slice::from_ref(&pruned), u64::MAX);
            assert_eq!(spans(&out), vec![4..104, 204..404]);
        }

        #[test]
        fn a_wide_early_blocked_span_still_blocks_a_later_gap() {
            // Crafted interleaving: the blocked span with the nearest start
            // (60..70) ends before the gap 104..204, but an earlier-starting
            // one (50..150) reaches into it. The lookup must use the running
            // max end, not just the nearest start.
            let plan = vec![span(4..104), span(204..304), span(304..404)];
            let blocked = [60..70, 50..150];
            let out = coalesce_spans(plan, &blocked, u64::MAX);
            assert_eq!(spans(&out), vec![4..104, 204..404]);
        }

        #[test]
        fn a_blocked_span_starting_where_the_gap_ends_does_not_block_it() {
            // Crafted footer: pruned 204..250 shares bytes with selected
            // 204..304. The gap 104..204 is half-open, so the pruned span
            // does not reach into it, and its bytes are fetched for the
            // selected group anyway.
            let plan = vec![span(4..104), span(204..304)];
            let out = coalesce_spans(plan, std::slice::from_ref(&(204..250)), u64::MAX);
            assert_eq!(spans(&out), vec![4..304]);
        }

        #[test]
        fn out_of_order_input_is_merged_in_file_order() {
            let plan = vec![span(204..304), span(4..104), span(104..204)];
            let out = coalesce_spans(plan, &[], u64::MAX);
            assert_eq!(spans(&out), vec![4..304]);
            let starts: Vec<u64> = out[0].chunks.iter().map(|c| c.0).collect();
            assert!(starts.windows(2).all(|w| w[0] < w[1]), "{starts:?}");
        }

        #[test]
        fn an_oversized_span_stays_a_single_request() {
            let plan = vec![span(4..504), span(504..554), span(554..604)];
            let out = coalesce_spans(plan, &[], 100);
            assert_eq!(spans(&out), vec![4..504, 504..604]);
        }

        #[test]
        fn a_gap_between_selected_spans_is_bridged_only_within_the_window() {
            // Unowned bytes (no row group) between two selected spans, e.g.
            // padding: bridging them is safe, but they count toward the window.
            let plan = vec![span(4..104), span(120..220)];
            assert_eq!(spans(&coalesce_spans(plan, &[], 216)), vec![4..220]);
            let plan = vec![span(4..104), span(120..220)];
            assert_eq!(
                spans(&coalesce_spans(plan, &[], 215)),
                vec![4..104, 120..220]
            );
        }

        /// `groups` row groups of `rows` rows each.
        #[cfg(test)]
        fn multi_rg_parquet(rows: usize, groups: usize) -> Vec<u8> {
            use arrow_array::{Int64Array, RecordBatch};
            use std::sync::Arc;
            use tylertoo_io::test_support::WriterProperties;

            let props = WriterProperties::builder()
                .set_max_row_group_row_count(Some(rows))
                .build();
            let values: Vec<i64> = (0..(rows * groups) as i64).collect();
            let batch =
                RecordBatch::try_from_iter([("v", Arc::new(Int64Array::from(values)) as _)])
                    .unwrap();
            let mut buf = Vec::new();
            let mut w = tylertoo_io::test_support::ArrowWriter::try_new(
                &mut buf,
                batch.schema(),
                Some(props),
            )
            .unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            buf
        }

        #[test]
        fn replaced_object_is_rejected_by_direct_and_staged_reads() {
            use object_store::{memory::InMemory, path::Path, ObjectStoreExt};
            use std::sync::Arc;
            for staged in [false, true] {
                let store = Arc::new(InMemory::new());
                let location = Path::from("versioned.parquet");
                let original = multi_rg_parquet(10, 2);
                super::runtime()
                    .block_on(store.put(&location, original.clone().into()))
                    .unwrap();
                let source = super::RemoteSource::from_store(
                    store.clone(),
                    location.clone(),
                    "memory://versioned.parquet".into(),
                )
                .unwrap();
                source.load_footer().unwrap();
                // Keep the footer and physical layout identical; only the store's
                // object version changes between HEAD/footer and the data GET.
                super::runtime()
                    .block_on(store.put(&location, original.into()))
                    .unwrap();
                let err = if staged {
                    source
                        .stage_row_groups_with(None, &tunables(0))
                        .unwrap_err()
                        .to_string()
                } else {
                    source.reader().fetch(4..12).unwrap_err().to_string()
                };
                assert!(
                    err.contains("Precondition") || err.contains("precondition"),
                    "{err}"
                );
            }
        }

        fn tunables(window: u64) -> StagingTunables {
            StagingTunables {
                coalesce_window: window,
                ..StagingTunables::default()
            }
        }

        /// Stage through the seam; return (requests, ranges) the staging added.
        fn stage(
            source: &InputSource,
            selected: Option<&[usize]>,
            window: u64,
        ) -> (u64, Vec<Range<u64>>) {
            let InputSource::Remote(r) = source else {
                unreachable!()
            };
            let _ = source.open().unwrap(); // footer first, so it is not counted
            let before = source.fetch_stats().unwrap().requests;
            let ranges_before = source.fetched_ranges().unwrap().len();
            r.stage_row_groups_with(selected, &tunables(window))
                .unwrap();
            let after = source.fetch_stats().unwrap().requests;
            let ranges = source.fetched_ranges().unwrap()[ranges_before..].to_vec();
            (after - before, ranges)
        }

        #[test]
        fn a_large_window_stages_every_row_group_in_one_request() {
            let bytes = multi_rg_parquet(1000, 5);
            let source = crate::input::test_memory_source(bytes, "w.parquet");
            let (requests, _) = stage(&source, None, u64::MAX);
            assert_eq!(requests, 1, "5 contiguous row groups, one window");

            // The passes then read entirely from the spill: no more requests,
            // and every value comes back.
            let before = source.fetch_stats().unwrap().requests;
            let reader = source.open().unwrap().build().unwrap();
            let mut sum = 0i64;
            for batch in reader {
                let batch = batch.unwrap();
                let col = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<arrow_array::Int64Array>()
                    .unwrap();
                sum += col.iter().flatten().sum::<i64>();
            }
            assert_eq!(sum, (0..5000i64).sum::<i64>());
            assert_eq!(
                source.fetch_stats().unwrap().requests,
                before,
                "staged chunks must serve the read without the network"
            );
        }

        #[test]
        fn window_zero_keeps_one_request_per_row_group_end_to_end() {
            let bytes = multi_rg_parquet(1000, 5);
            let source = crate::input::test_memory_source(bytes, "w0.parquet");
            let (requests, _) = stage(&source, None, 0);
            assert_eq!(requests, 5);
        }

        #[test]
        fn a_large_window_never_fetches_a_pruned_row_group() {
            let bytes = multi_rg_parquet(1000, 5);
            let md =
                tylertoo_io::read_parquet_metadata(&bytes::Bytes::from(bytes.clone())).unwrap();
            let pruned = md.row_group(2);
            let lo = pruned
                .columns()
                .iter()
                .map(|c| c.byte_range().0)
                .min()
                .unwrap();
            let hi = pruned
                .columns()
                .iter()
                .map(|c| c.byte_range().0 + c.byte_range().1)
                .max()
                .unwrap();

            let source = crate::input::test_memory_source(bytes, "wp.parquet");
            let (requests, ranges) = stage(&source, Some(&[0, 1, 3, 4]), u64::MAX);
            assert_eq!(requests, 2, "0+1 and 3+4, split at the pruned group");
            for r in ranges {
                assert!(
                    r.end <= lo || r.start >= hi,
                    "range {r:?} overlaps pruned row group {lo}..{hi}"
                );
            }
        }
    }

    #[cfg(test)]
    mod client_override_tests {
        //! #290 dimension 4: `TYLERTOO_HTTP_<KEY>` → `object_store` client option.

        use super::{client_overrides, overrides_log_line};
        use object_store::ClientConfigKey;

        fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        }

        #[test]
        fn no_vars_means_no_overrides() {
            let (o, w) = client_overrides(vars(&[("PATH", "/bin"), ("AWS_REGION", "x")]));
            assert!(o.is_empty() && w.is_empty(), "{o:?} {w:?}");
        }

        #[test]
        fn known_keys_map_to_client_config_keys() {
            let (o, w) = client_overrides(vars(&[
                ("TYLERTOO_HTTP_HTTP2_ONLY", "true"),
                ("TYLERTOO_HTTP_POOL_MAX_IDLE_PER_HOST", "4"),
            ]));
            assert!(w.is_empty(), "{w:?}");
            assert!(o.contains(&(ClientConfigKey::Http2Only, "true".to_string())));
            assert!(o.contains(&(ClientConfigKey::PoolMaxIdlePerHost, "4".to_string())));
        }

        #[test]
        fn key_suffix_is_case_insensitive() {
            let (o, w) = client_overrides(vars(&[("TYLERTOO_HTTP_http1_only", "true")]));
            assert!(w.is_empty(), "{w:?}");
            assert_eq!(o, vec![(ClientConfigKey::Http1Only, "true".to_string())]);
        }

        #[test]
        fn unknown_keys_warn_naming_the_variable_and_are_skipped() {
            let (o, w) = client_overrides(vars(&[("TYLERTOO_HTTP_WARP_SPEED", "9")]));
            assert!(o.is_empty(), "{o:?}");
            assert_eq!(w.len(), 1);
            assert!(w[0].contains("TYLERTOO_HTTP_WARP_SPEED"), "{}", w[0]);
        }

        #[test]
        fn a_bad_proxy_url_warning_does_not_echo_credentials() {
            let (o, w) = client_overrides(vars(&[(
                "TYLERTOO_HTTP_PROXY_URL",
                "http://user:hunter2@[::1:3128",
            )]));
            assert!(o.is_empty(), "{o:?}");
            assert_eq!(w.len(), 1, "{w:?}");
            assert!(w[0].contains("TYLERTOO_HTTP_PROXY_URL"), "{}", w[0]);
            assert!(!w[0].contains("hunter2"), "{}", w[0]);
        }

        #[test]
        fn malformed_values_warn_naming_the_variable_and_are_skipped() {
            let (o, w) = client_overrides(vars(&[
                ("TYLERTOO_HTTP_TIMEOUT", "soon"),
                ("TYLERTOO_HTTP_HTTP2_ONLY", "perhaps"),
                ("TYLERTOO_HTTP_CONNECT_TIMEOUT", "10s"),
            ]));
            assert_eq!(
                o,
                vec![(ClientConfigKey::ConnectTimeout, "10s".to_string())]
            );
            assert_eq!(w.len(), 2, "{w:?}");
            assert!(
                w.iter().any(|m| m.contains("TYLERTOO_HTTP_TIMEOUT")),
                "{w:?}"
            );
            assert!(
                w.iter().any(|m| m.contains("TYLERTOO_HTTP_HTTP2_ONLY")),
                "{w:?}"
            );
        }

        #[test]
        fn a_bad_value_warning_echoes_the_value_unless_redacted() {
            // Only PROXY_URL is withheld; any other bad value is shown so the
            // user can see what they typed.
            let (_, w) = client_overrides(vars(&[("TYLERTOO_HTTP_TIMEOUT", "soon")]));
            assert_eq!(w.len(), 1, "{w:?}");
            assert!(w[0].contains("TYLERTOO_HTTP_TIMEOUT=soon"), "{}", w[0]);
        }

        #[test]
        fn output_is_sorted_so_the_log_line_is_stable() {
            let (o, _) = client_overrides(vars(&[
                ("TYLERTOO_HTTP_TIMEOUT", "60s"),
                ("TYLERTOO_HTTP_CONNECT_TIMEOUT", "10s"),
            ]));
            let keys: Vec<String> = o.iter().map(|(k, _)| k.as_ref().to_string()).collect();
            assert_eq!(keys, vec!["connect_timeout", "timeout"]);
        }

        #[test]
        fn no_overrides_means_no_log_line() {
            assert_eq!(overrides_log_line(&[]), None);
        }

        #[test]
        fn log_line_names_each_override_and_redacts_the_proxy_url() {
            let line = overrides_log_line(&[
                (
                    ClientConfigKey::ProxyUrl,
                    "http://user:hunter2@proxy:3128".to_string(),
                ),
                (ClientConfigKey::Timeout, "60s".to_string()),
            ])
            .expect("overrides are in force");
            assert!(line.contains("TYLERTOO_HTTP_*"), "{line}");
            assert!(line.contains("proxy_url=<redacted>"), "{line}");
            assert!(line.contains("timeout=60s"), "{line}");
            assert!(!line.contains("hunter2"), "{line}");
        }
    }

    #[cfg(test)]
    mod process_env_tests {
        //! The two `OnceLock`s that read the real process environment
        //! ([`staging_tunables::effective`], [`effective_client_overrides`])
        //! and the store each scheme resolves to. An environment test re-runs
        //! itself in a child process with its variables set, so no test
        //! mutates this process's environment and the cache is filled from
        //! exactly those variables.

        use super::staging_tunables::{self, COALESCE_VAR, CONCURRENCY_VAR};
        use super::{effective_client_overrides, store_and_location};
        use object_store::ClientConfigKey;

        /// Runs the named test of this module alone in a child process with
        /// `vars` set. Returns `true` inside that child, where the caller runs
        /// its body, and `false` in the parent once the child has passed.
        /// Same shape as `run_alone` in `overview::stream`.
        fn run_in_child(test: &str, vars: &[(&str, &str)]) -> bool {
            const CHILD: &str = "TYLERTOO_ENV_TEST_CHILD";
            if std::env::var_os(CHILD).is_some() {
                return true;
            }
            let (_, module) = module_path!().split_once("::").expect("crate::module path");
            let name = format!("{module}::{test}");
            let out =
                std::process::Command::new(std::env::current_exe().expect("test binary path"))
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

        #[test]
        fn live_staging_concurrency_is_logged_for_each_operation() {
            if !run_in_child("live_staging_concurrency_is_logged_for_each_operation", &[]) {
                return;
            }
            struct Capture;
            static LINES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
            impl log::Log for Capture {
                fn enabled(&self, _: &log::Metadata<'_>) -> bool {
                    true
                }
                fn log(&self, record: &log::Record<'_>) {
                    LINES.lock().unwrap().push(record.args().to_string());
                }
                fn flush(&self) {}
            }
            log::set_logger(&Capture).unwrap();
            log::set_max_level(log::LevelFilter::Info);
            let (mut tunables, _) = staging_tunables::resolve(|_| None);
            tunables.overridden = true;
            staging_tunables::log_live_concurrency(&tunables, 2, 4096);
            staging_tunables::log_live_concurrency(&tunables, 3, 8192);
            let lines = LINES.lock().unwrap();
            assert_eq!(
                lines.len(),
                2,
                "each operation must report its own live concurrency"
            );
            assert!(lines[0].contains("live concurrency 2"));
            assert!(lines[1].contains("live concurrency 3"));
        }

        #[test]
        fn effective_staging_tunables_read_the_environment() {
            if !run_in_child(
                "effective_staging_tunables_read_the_environment",
                &[(CONCURRENCY_VAR, "3"), (COALESCE_VAR, "4096")],
            ) {
                return;
            }
            let t = staging_tunables::effective();
            assert_eq!(t.max_concurrency, 3);
            assert_eq!(t.coalesce_window, 4096);
            assert!(t.overridden);
        }

        #[test]
        fn effective_client_overrides_read_the_environment() {
            if !run_in_child(
                "effective_client_overrides_read_the_environment",
                &[("TYLERTOO_HTTP_TIMEOUT", "7s")],
            ) {
                return;
            }
            assert!(
                effective_client_overrides()
                    .contains(&(ClientConfigKey::Timeout, "7s".to_string())),
                "{:?}",
                effective_client_overrides()
            );
        }

        #[test]
        fn gs_urls_resolve_to_a_store() {
            // Building the store does no I/O: credentials resolve on the
            // first request.
            let (_, location) = store_and_location("gs://some-bucket/dir/part.parquet")
                .expect("gs:// builds a GCS store");
            assert_eq!(location.as_ref(), "dir/part.parquet");
        }
    }

    #[cfg(test)]
    mod spill_dir_tests {
        use super::*;

        /// #272: a configured spill directory is where the spill file is
        /// created. The file is anonymous (unlinked on creation), so the
        /// dir is observed through behavior: with a valid dir the put/get
        /// roundtrip works end to end.
        #[test]
        fn disk_spill_writes_into_configured_dir() {
            let dir = tempfile::tempdir().unwrap();
            let mut spill = DiskSpill {
                dir: Some(dir.path().to_path_buf()),
                ..DiskSpill::default()
            };
            spill.put(0, &Bytes::from_static(b"hello spill"));
            assert_eq!(
                spill.get(0).as_deref(),
                Some(&b"hello spill"[..]),
                "spill roundtrip through the configured dir"
            );
        }

        /// #272 counterpart: a nonexistent configured dir makes file
        /// creation fail, which disables the spill (best-effort, #219) —
        /// proof the configured dir, not $TMPDIR, is what `put` uses.
        #[test]
        fn disk_spill_nonexistent_dir_disables_spill() {
            let mut spill = DiskSpill {
                dir: Some(std::path::PathBuf::from(
                    "/nonexistent/tylertoo-spill-dir-272",
                )),
                ..DiskSpill::default()
            };
            spill.put(0, &Bytes::from_static(b"hello spill"));
            assert!(
                spill.get(0).is_none(),
                "create failure must disable the spill"
            );
        }

        /// #273: a RAM-backed spill directory produces a warning naming the
        /// directory and pointing at `--spill-dir`. `/dev/shm` is tmpfs on
        /// essentially every Linux system.
        #[cfg(target_os = "linux")]
        #[test]
        fn ram_backed_dir_warns_naming_dir_and_flag() {
            let shm = std::path::Path::new("/dev/shm");
            if !shm.exists() {
                eprintln!("skipping ram_backed_dir_warns: /dev/shm absent");
                return;
            }
            let msg = ram_backed_spill_warning(shm)
                .expect("/dev/shm must produce a RAM-backed spill warning");
            assert!(msg.contains("/dev/shm"), "names the directory: {msg}");
            assert!(msg.contains("--spill-dir"), "suggests --spill-dir: {msg}");
            assert!(
                msg.contains("RAM-backed"),
                "explains the RAM-backed hazard: {msg}"
            );
        }

        /// #273 counterpart: a real-disk directory is not flagged, so the
        /// spill path stays silent and functional.
        #[test]
        fn real_disk_dir_does_not_warn() {
            let dir = tempfile::tempdir().unwrap();
            // Best-effort: on a machine whose $TMPDIR is tmpfs this could be
            // Some(_); guard on the detection result to stay non-flaky.
            if crate::fs_probe::is_ram_backed(dir.path()) == Some(true) {
                eprintln!("skipping real_disk_dir_does_not_warn: tempdir is tmpfs");
                return;
            }
            assert!(
                ram_backed_spill_warning(dir.path()).is_none(),
                "a non-RAM-backed dir must not warn"
            );
        }
    }

    #[cfg(test)]
    mod hint_tests {
        use super::*;

        fn signature_shaped_error() -> object_store::Error {
            object_store::Error::Generic {
                store: "S3",
                source: "Client error with status 403 Forbidden: \
                         <Code>InvalidAccessKeyId</Code>"
                    .into(),
            }
        }

        /// PR-C: a signature/authorization-style failure against a custom
        /// S3-compatible endpoint appends the `AWS_SKIP_SIGNATURE` hint in
        /// the error text (string-level; the error types are unchanged).
        #[test]
        fn signature_error_on_custom_endpoint_appends_skip_signature_hint() {
            let msg = store_error_with_hint("s3://b/k.parquet", signature_shaped_error(), true)
                .to_string();
            assert!(
                msg.contains("AWS_SKIP_SIGNATURE=true"),
                "hint appended: {msg}"
            );
            assert!(msg.contains("s3://b/k.parquet"), "keeps the URL: {msg}");
            assert!(msg.contains("403"), "keeps the original error: {msg}");
        }

        /// Without a custom endpoint the error is untouched (the hint is
        /// specific to S3-compatible endpoints like anonymous data hosts).
        #[test]
        fn signature_error_without_custom_endpoint_is_unchanged() {
            let msg = store_error_with_hint("s3://b/k.parquet", signature_shaped_error(), false)
                .to_string();
            assert!(!msg.contains("AWS_SKIP_SIGNATURE"), "no hint: {msg}");
            assert!(msg.contains("403"), "original error preserved: {msg}");
        }

        /// A non-signature failure (e.g. object not found) never grows the
        /// hint, custom endpoint or not.
        #[test]
        fn non_signature_error_never_hints() {
            let not_found = object_store::Error::NotFound {
                path: "k.parquet".to_string(),
                source: "no such key".into(),
            };
            let msg = store_error_with_hint("s3://b/k.parquet", not_found, true).to_string();
            assert!(!msg.contains("AWS_SKIP_SIGNATURE"), "no hint: {msg}");
        }
    }
}

/// Test-only: an [`InputSource`] over an in-memory object store seeded with
/// `bytes` — the seam remote tests (here and in `overview::convert`) inject
/// data through without touching the network.
#[cfg(all(test, feature = "remote"))]
pub(crate) fn test_memory_source(bytes: Vec<u8>, name: &str) -> InputSource {
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::ObjectStoreExt;
    use std::sync::Arc;

    let store = Arc::new(InMemory::new());
    let location = ObjectPath::from(name);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(store.put(&location, bytes.into())).unwrap();
    InputSource::Remote(
        remote::RemoteSource::from_store(store, location, format!("memory://{name}")).unwrap(),
    )
}

/// Fuzz hook (#633): read `bytes` as a remote GeoParquet object, through the
/// same [`remote::RemoteSource`] a `s3://`/`gs://`/`https://` input uses, with
/// an in-memory object store standing in for the network.
///
/// Covers the ranged footer fetch and its chunk-range checks (#430, #586),
/// row-group staging, and a full read of every row group through the
/// readahead-clamped reader. Returns the rows read. Only the `fuzz/` crate
/// calls this; the `fuzzing` and `remote` features are never both on in a
/// normal build.
#[cfg(all(feature = "fuzzing", feature = "remote"))]
#[doc(hidden)]
pub fn fuzz_remote_footer(bytes: &[u8]) -> Result<usize, InputError> {
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::ObjectStoreExt;
    #[expect(
        clippy::disallowed_types,
        reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
    )]
    use std::sync::{Arc, OnceLock};

    #[expect(
        clippy::disallowed_types,
        reason = "#779: runtime, client and per-input caches; never diagnostic suppression"
    )]
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    let rt = RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("tokio runtime for the fuzz hook")
    });
    let store = Arc::new(InMemory::new());
    let location = ObjectPath::from("fuzz.parquet");
    rt.block_on(store.put(&location, bytes.to_vec().into()))
        .expect("an in-memory put cannot fail");
    let source = InputSource::Remote(remote::RemoteSource::from_store(
        store,
        location,
        "memory://fuzz.parquet".to_string(),
    )?);
    source.stage_row_groups(None)?;
    let mut rows = 0;
    for batch in source.open()?.with_batch_rows(256).build()? {
        rows += batch?.num_rows();
    }
    source.release_read_cache();
    Ok(rows)
}

/// Test-only: a multi-partition [`crate::input_set::ConvertSource`] over ONE
/// in-memory object store — `objects` are `(basename, bytes)` pairs placed
/// under a `set/` prefix and resolved through the REAL prefix-listing path
/// (filtering, key sort, shared store instance, sizes from the listing).
/// Also returns the per-part [`InputSource`] handles: clones share the fetch
/// counters, so tests can assert per-part fetched ranges after a convert.
#[cfg(all(test, feature = "remote"))]
pub(crate) fn test_memory_multi_source(
    objects: Vec<(&str, Vec<u8>)>,
) -> (crate::input_set::ConvertSource, Vec<InputSource>) {
    use crate::input_set::{ConvertSource, MultiSource};
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{ObjectStore, ObjectStoreExt};
    use std::sync::Arc;

    let store = Arc::new(InMemory::new());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for (name, bytes) in objects {
        rt.block_on(store.put(&ObjectPath::from(format!("set/{name}")), bytes.into()))
            .unwrap();
    }
    let store: Arc<dyn ObjectStore> = store;
    let listed =
        remote::list_parquet_under_prefix(&store, &ObjectPath::from("set"), "memory://set/")
            .unwrap();
    let parts: Vec<InputSource> = listed
        .into_iter()
        .map(|p| {
            InputSource::Remote(remote::RemoteSource::from_store_meta(
                Arc::clone(&store),
                format!("memory://{}", p.location),
                p,
            ))
        })
        .collect();
    let source = if parts.len() == 1 {
        ConvertSource::single(parts[0].clone())
    } else {
        ConvertSource::Multi(
            MultiSource::from_sources("memory://set/".to_string(), parts.clone()).unwrap(),
        )
    };
    (source, parts)
}

/// Test-only: [`test_memory_source`] with an explicit chunk-cache floor, so a
/// small fixture whose row group exceeds `cap_base` exercises the eviction /
/// re-fetch path at unit-test scale (issue #261 regression coverage).
#[cfg(all(test, feature = "remote"))]
pub(crate) fn test_memory_source_with_cap(
    bytes: Vec<u8>,
    name: &str,
    cap_base: u64,
) -> InputSource {
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::ObjectStoreExt;
    use std::sync::Arc;

    let store = Arc::new(InMemory::new());
    let location = ObjectPath::from(name);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(store.put(&location, bytes.into())).unwrap();
    InputSource::Remote(
        remote::RemoteSource::from_store_with_cap_base(
            store,
            location,
            format!("memory://{name}"),
            cap_base,
        )
        .unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_path_is_local() {
        let s = InputSource::from_path(Path::new("/tmp/foo.parquet")).unwrap();
        assert!(!s.is_remote());
        assert!(s.fetch_stats().is_none());
    }

    #[test]
    fn relative_path_is_local() {
        let s = InputSource::from_str_input("data/foo.parquet").unwrap();
        assert!(!s.is_remote());
    }

    #[test]
    fn file_url_maps_to_local() {
        let s = InputSource::from_str_input("file:///tmp/foo.parquet").unwrap();
        match s {
            InputSource::Local(p) => assert_eq!(p, PathBuf::from("/tmp/foo.parquet")),
            #[cfg(feature = "remote")]
            InputSource::Remote(_) => panic!("file:// must be local"),
        }
    }

    #[test]
    fn unsupported_scheme_is_a_helpful_error() {
        let err = InputSource::from_str_input("ftp://example.com/foo.parquet").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ftp"), "message names the scheme: {msg}");
        assert!(msg.contains("s3://"), "message lists alternatives: {msg}");
    }

    /// #452: a missing input used to surface as a bare "io error: No such
    /// file or directory", with nothing saying which file.
    #[test]
    fn opening_a_missing_file_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.parquet");
        let err = InputSource::from_path(&path).unwrap().open().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&path.display().to_string()),
            "names the path: {msg}"
        );
        assert!(msg.contains("No such file"), "keeps the OS reason: {msg}");
    }

    /// #452: a corrupt footer read "parquet error: Parquet error: Corrupt
    /// footer", with no file name and the prefix said twice.
    #[test]
    fn a_corrupt_footer_names_the_file_without_stutter() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.parquet");
        std::fs::write(&path, b"not a parquet file at all, just text").unwrap();
        let err = InputSource::from_path(&path).unwrap().open().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains(&path.display().to_string()),
            "names the path: {msg}"
        );
        assert!(
            msg.contains("Corrupt footer"),
            "keeps parquet's reason: {msg}"
        );
        assert_eq!(
            msg.to_lowercase().matches("parquet error").count(),
            0,
            "no 'parquet error:' prefix, let alone two: {msg}"
        );
    }

    /// #452: a mid-read Parquet error says "parquet error" once.
    #[test]
    fn a_parquet_error_says_so_once() {
        let err = InputError::from(ParquetError::General("bad page".into()));
        assert_eq!(err.to_string(), "parquet error: bad page");
    }

    #[test]
    fn windows_style_drive_is_local() {
        // `C:\...` has a colon but no `://`; must not be parsed as a URL.
        let s = InputSource::from_str_input(r"C:\data\foo.parquet").unwrap();
        assert!(!s.is_remote());
    }

    #[cfg(not(feature = "remote"))]
    #[test]
    fn remote_url_without_feature_is_a_clear_error() {
        let err = InputSource::from_str_input("s3://bucket/key.parquet").unwrap_err();
        assert!(err.to_string().contains("remote"), "err: {err}");
    }

    /// A footer whose row groups declare `sizes` compressed bytes per
    /// column chunk, two `INT64` columns each, and nothing else.
    fn footer_with_chunk_sizes(sizes: &[[i64; 2]]) -> tylertoo_io::ParquetMetaData {
        use std::sync::Arc;
        use tylertoo_io::test_support::{
            ColumnChunkMetaData, FileMetaData, ParquetMetaData, RowGroupMetaData,
        };
        use tylertoo_io::test_support::{SchemaDescriptor, Type};
        use tylertoo_io::PhysicalType;

        let fields = ["a", "b"]
            .map(|n| {
                Arc::new(
                    Type::primitive_type_builder(n, PhysicalType::INT64)
                        .build()
                        .unwrap(),
                )
            })
            .to_vec();
        let schema = Type::group_type_builder("schema")
            .with_fields(fields)
            .build()
            .unwrap();
        let descr = Arc::new(SchemaDescriptor::new(Arc::new(schema)));
        let groups = sizes
            .iter()
            .map(|chunks| {
                let columns = chunks
                    .iter()
                    .enumerate()
                    .map(|(i, &size)| {
                        ColumnChunkMetaData::builder(descr.column(i))
                            .set_total_compressed_size(size)
                            .build()
                            .unwrap()
                    })
                    .collect();
                RowGroupMetaData::builder(descr.clone())
                    .set_num_rows(1)
                    .set_column_metadata(columns)
                    .build()
                    .unwrap()
            })
            .collect();
        let rows = sizes.len() as i64;
        ParquetMetaData::new(FileMetaData::new(2, rows, None, None, descr, None), groups)
    }

    /// #686: a footer is outside data, and its chunk sizes are whatever it
    /// says. parquet-rs's `RowGroupMetaData::compressed_size` sums them
    /// unchecked (a panic in a debug build, a wrap in release), and the
    /// total across row groups overflowed tylertoo's own `u64` sum, which
    /// panics in release too (#432's overflow checks).
    #[test]
    fn selected_compressed_bytes_saturates_on_a_hostile_footer() {
        let md = footer_with_chunk_sizes(&[[i64::MAX, i64::MAX]; 3]);
        assert_eq!(selected_compressed_bytes(&md, None), u64::MAX);
        assert_eq!(selected_compressed_bytes(&md, Some(&[0, 1, 2])), u64::MAX);
        // Negative sizes count as nothing, as before.
        let md = footer_with_chunk_sizes(&[[-5, 7], [3, -1]]);
        assert_eq!(selected_compressed_bytes(&md, None), 10);
    }

    #[cfg(feature = "remote")]
    mod remote_tests {
        use super::super::*;

        use super::super::remote::RemoteSource;
        use super::super::test_memory_source as memory_source;
        use object_store::path::Path as ObjectPath;
        use object_store::{ObjectMeta, ObjectStore};
        use std::sync::Arc;

        /// Minimal single-column parquet bytes for reader plumbing tests.
        fn tiny_parquet() -> Vec<u8> {
            use arrow_array::{Int64Array, RecordBatch};
            use std::sync::Arc as SArc;
            use tylertoo_io::test_support::ArrowWriter;

            let batch = RecordBatch::try_from_iter([(
                "v",
                SArc::new(Int64Array::from(vec![1i64, 2, 3])) as _,
            )])
            .unwrap();
            let mut buf = Vec::new();
            let mut w = ArrowWriter::try_new(&mut buf, batch.schema(), None).unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            buf
        }

        #[test]
        fn remote_reader_roundtrips_parquet() {
            let bytes = tiny_parquet();
            let total = bytes.len() as u64;
            let source = memory_source(bytes, "tiny.parquet");
            assert!(source.is_remote());

            let builder = source.open().unwrap();
            let reader = builder.build().unwrap();
            let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
            assert_eq!(rows, 3);

            let stats = source.fetch_stats().unwrap();
            assert_eq!(stats.object_size, total);
            assert!(stats.requests >= 2, "footer + data: {stats:?}");
            assert!(stats.bytes_fetched > 0);
            // Every individual range stays within the object (requests may
            // overlap each other: footer suffix then full footer).
            for r in source.fetched_ranges().unwrap() {
                assert!(r.end <= total, "range {r:?} beyond object size {total}");
            }
        }

        #[test]
        fn footer_is_cached_across_opens() {
            let source = memory_source(tiny_parquet(), "tiny.parquet");
            let _ = source.open().unwrap();
            let after_first = source.fetch_stats().unwrap();
            let _ = source.open().unwrap();
            let after_second = source.fetch_stats().unwrap();
            assert_eq!(
                after_first.requests, after_second.requests,
                "second open must not re-fetch the footer"
            );
        }

        #[test]
        fn sequential_read_matches_object_bytes() {
            use std::io::Read;
            use tylertoo_io::ChunkReader;

            let bytes = tiny_parquet();
            let source = memory_source(bytes.clone(), "tiny.parquet");
            let InputSource::Remote(ref r) = source else {
                unreachable!()
            };
            let reader = InputReader::Remote(r.reader());
            let mut out = Vec::new();
            reader.get_read(4).unwrap().read_to_end(&mut out).unwrap();
            assert_eq!(out, &bytes[4..]);
        }

        /// Anonymous HTTPS against a public object (a GitHub release asset,
        /// which serves range requests): the footer must be readable with a
        /// partial fetch and no credentials. Skips (passing trivially,
        /// loudly) when the network is unavailable.
        #[test]
        fn https_public_object_integration() {
            const URL: &str = "https://github.com/geoparquet-io/tylertoo/releases/download/fixtures-v1/fieldmaps-boundaries.parquet";
            let source = match InputSource::from_str_input(URL) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("SKIP https_public_object_integration (no network?): {e}");
                    return;
                }
            };
            let builder = match source.open() {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("SKIP https_public_object_integration (no network?): {e}");
                    return;
                }
            };
            assert!(
                builder
                    .schema()
                    .fields()
                    .iter()
                    .any(|f| f.name() == "geometry"),
                "public GeoParquet fixture has a geometry column"
            );
            let stats = source.fetch_stats().unwrap();
            assert!(stats.bytes_fetched > 0);
            assert!(
                stats.bytes_fetched < stats.object_size,
                "footer open must be a partial fetch: {stats:?}"
            );
        }

        /// A two-column parquet with many small data pages in one row group,
        /// so the arrow reader interleaves the wide column with the narrow one
        /// across batches and touches the wide chunk's pages repeatedly — the
        /// access pattern that made issue #261's oversized geometry chunk
        /// re-fetch per page. Dictionary encoding is off and the strings are
        /// distinct so the wide column stays genuinely wide.
        fn two_column_multipage_parquet(rows: usize) -> Vec<u8> {
            use arrow_array::{Int64Array, RecordBatch, StringArray};
            use std::sync::Arc as SArc;
            use tylertoo_io::test_support::WriterProperties;

            let wide: Vec<String> = (0..rows)
                .map(|i| format!("feature-{i:08}-{:-<48}", i % 7))
                .collect();
            let narrow: Vec<i64> = (0..rows as i64).collect();
            let batch = RecordBatch::try_from_iter([
                ("geo", SArc::new(StringArray::from(wide)) as _),
                ("tag", SArc::new(Int64Array::from(narrow)) as _),
            ])
            .unwrap();
            let props = WriterProperties::builder()
                .set_dictionary_enabled(false)
                .set_data_page_row_count_limit(128)
                .build();
            let mut buf = Vec::new();
            let mut w = tylertoo_io::test_support::ArrowWriter::try_new(
                &mut buf,
                batch.schema(),
                Some(props),
            )
            .unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            buf
        }

        /// #261 regression: when a row group's working set exceeds the chunk
        /// cache floor, a single in-order read must still move ≈ the object's
        /// bytes, not re-fetch the wide column per page. With a tiny `cap_base`
        /// the pre-fix cache evicted the wide chunk on insert and re-fetched it
        /// on every page read (measured 96× on a vertex-heavy input); the
        /// footer-sized cap keeps the row group's chunks resident.
        #[test]
        fn oversized_row_group_is_not_refetched_per_page() {
            let bytes = two_column_multipage_parquet(4096);
            let object_size = bytes.len() as u64;
            // Floor far below one row group, forcing the eviction path.
            let source = super::super::test_memory_source_with_cap(bytes, "wide.parquet", 64);

            let reader = source.open().unwrap().with_batch_rows(64).build().unwrap();
            let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
            assert_eq!(rows, 4096);

            let stats = source.fetch_stats().unwrap();
            assert_eq!(stats.object_size, object_size);
            // One in-order pass moves the object once (plus footer overhead),
            // never a multiple of it. Pre-fix this ratio was many×.
            assert!(
                stats.bytes_fetched <= object_size + object_size / 2,
                "single read re-fetched the input: moved {} bytes for a {}-byte \
                 object ({:.1}×) — oversized chunk evicted mid-read (#261)",
                stats.bytes_fetched,
                object_size,
                stats.bytes_fetched as f64 / object_size as f64,
            );
        }

        /// #219: across multiple passes over a remote input, each byte must
        /// move over the network at most once. The streaming converter reads
        /// the input several times (assign pass, coarse-level pass, finest
        /// streamed last); the in-memory chunk cache holds only one row group's
        /// working set, so without a local spill each pass re-fetches the bulk
        /// of the file (measured 3.0× on fieldmaps-adm4). The disk spill drains
        /// re-reads from local disk, bounding remote traffic to ≈1× the object
        /// regardless of pass count. With a `cap_base` far below one row group
        /// the in-memory cache cannot bridge passes, so only the spill keeps the
        /// ratio down.
        #[test]
        fn multi_pass_reads_move_object_once() {
            let bytes = multi_row_group_wide_parquet(4096, 3);
            let object_size = bytes.len() as u64;
            let source = super::super::test_memory_source_with_cap(bytes, "spill.parquet", 64);

            // Three full in-order passes, mimicking the converter's assign +
            // coarse-level + finest-streamed-last reads.
            for _ in 0..3 {
                let reader = source.open().unwrap().with_batch_rows(64).build().unwrap();
                let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
                assert_eq!(rows, 4096 * 3);
            }

            let stats = source.fetch_stats().unwrap();
            assert_eq!(stats.object_size, object_size);
            // Every column chunk is fetched over the network exactly once; the
            // second and third passes drain from the local spill. Pre-spill,
            // three passes moved ≈3× the object.
            assert!(
                stats.bytes_fetched <= object_size + object_size / 2,
                "three passes moved {} bytes for a {}-byte object ({:.1}×) — the \
                 disk spill must serve re-reads locally (#219)",
                stats.bytes_fetched,
                object_size,
                stats.bytes_fetched as f64 / object_size as f64,
            );

            // No column-chunk range is fetched twice: re-touches hit the spill.
            let fetched = source.fetched_ranges().unwrap();
            let mut seen = std::collections::HashSet::new();
            for r in &fetched {
                assert!(
                    seen.insert((r.start, r.end)),
                    "range {r:?} fetched over the network more than once (#219)"
                );
            }
        }

        /// #272: `set_spill_dir` threads through [`InputSource`] →
        /// `RemoteSource` → `DiskSpill`. Observed through behavior: a
        /// deliberately broken spill dir disables the spill (best-effort,
        /// #219), so multi-pass reads degrade to per-pass network re-fetch —
        /// proof the configured dir, not `$TMPDIR`, is what the spill uses
        /// (with the default dir the same reads stay ≈1×, see
        /// [`multi_pass_reads_move_object_once`]).
        #[test]
        fn spill_dir_reaches_disk_spill_via_source() {
            let bytes = multi_row_group_wide_parquet(4096, 3);
            let object_size = bytes.len() as u64;
            let source = super::super::test_memory_source_with_cap(bytes, "spill-dir.parquet", 64);
            source.set_spill_dir(Some(Path::new("/nonexistent/tylertoo-spill-dir-272")));

            for _ in 0..3 {
                let reader = source.open().unwrap().with_batch_rows(64).build().unwrap();
                let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
                assert_eq!(rows, 4096 * 3);
            }

            let stats = source.fetch_stats().unwrap();
            assert!(
                stats.bytes_fetched >= 2 * object_size,
                "spill disabled by the broken dir: three passes must re-fetch \
                 (moved {} bytes for a {}-byte object) — is the configured dir \
                 actually reaching DiskSpill?",
                stats.bytes_fetched,
                object_size,
            );
        }

        /// #272 positive counterpart: with a valid caller-chosen spill dir,
        /// multi-pass reads keep the #219 ≈1× network bound.
        #[test]
        fn valid_spill_dir_keeps_one_pass_bound() {
            let bytes = multi_row_group_wide_parquet(4096, 3);
            let object_size = bytes.len() as u64;
            let source =
                super::super::test_memory_source_with_cap(bytes, "spill-dir-ok.parquet", 64);
            let dir = tempfile::tempdir().unwrap();
            source.set_spill_dir(Some(dir.path()));

            for _ in 0..3 {
                let reader = source.open().unwrap().with_batch_rows(64).build().unwrap();
                let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
                assert_eq!(rows, 4096 * 3);
            }

            let stats = source.fetch_stats().unwrap();
            assert!(
                stats.bytes_fetched <= object_size + object_size / 2,
                "three passes moved {} bytes for a {}-byte object ({:.1}×) — the \
                 spill in the configured dir must serve re-reads locally",
                stats.bytes_fetched,
                object_size,
                stats.bytes_fetched as f64 / object_size as f64,
            );
        }

        /// #272: the projected spill size is the Σ compressed bytes of the
        /// selected row groups, straight from the parquet footer. Signature
        /// is file-count-agnostic (metadata + selection → u64) so a
        /// multi-partition source can sum it across parts.
        #[test]
        fn selected_compressed_bytes_sums_selection() {
            let bytes = multi_row_group_wide_parquet(1024, 3);
            let source = memory_source(bytes, "selected-bytes.parquet");
            let builder = source.open().unwrap();
            let md = builder.metadata();
            let per_group: Vec<u64> = md
                .row_groups()
                .iter()
                .map(|rg| rg.compressed_size() as u64)
                .collect();
            assert_eq!(per_group.len(), 3);
            assert!(per_group.iter().all(|b| *b > 0));
            assert_eq!(
                selected_compressed_bytes(md, None),
                per_group.iter().sum::<u64>(),
                "no selection = whole file"
            );
            assert_eq!(
                selected_compressed_bytes(md, Some(&[0, 2])),
                per_group[0] + per_group[2],
                "selection sums only the selected row groups"
            );
            assert_eq!(selected_compressed_bytes(md, Some(&[])), 0);
        }

        /// Like [`two_column_multipage_parquet`] but split into `groups` row
        /// groups, so a full read walks several oversized row groups in
        /// sequence — exercising cross-row-group eviction (the cache must drop
        /// the previous group's chunks, not accumulate them).
        #[cfg(test)]
        fn multi_row_group_wide_parquet(rows_per_group: usize, groups: usize) -> Vec<u8> {
            use arrow_array::{Int64Array, RecordBatch, StringArray};
            use std::sync::Arc as SArc;
            use tylertoo_io::test_support::WriterProperties;

            let props = WriterProperties::builder()
                .set_dictionary_enabled(false)
                .set_data_page_row_count_limit(128)
                .set_max_row_group_row_count(Some(rows_per_group))
                .build();
            let total = rows_per_group * groups;
            let wide: Vec<String> = (0..total)
                .map(|i| format!("feature-{i:08}-{:-<48}", i % 7))
                .collect();
            let narrow: Vec<i64> = (0..total as i64).collect();
            let batch = RecordBatch::try_from_iter([
                ("geo", SArc::new(StringArray::from(wide)) as _),
                ("tag", SArc::new(Int64Array::from(narrow)) as _),
            ])
            .unwrap();
            let mut buf = Vec::new();
            let mut w = tylertoo_io::test_support::ArrowWriter::try_new(
                &mut buf,
                batch.schema(),
                Some(props),
            )
            .unwrap();
            w.write(&batch).unwrap();
            w.close().unwrap();
            buf
        }

        /// #261 benchmark: report the remote-fetch amplification (bytes moved ÷
        /// object size) for a single in-order read of an input whose row groups
        /// each exceed the chunk-cache floor. Shrinking the cache via `cap_base`
        /// reproduces the "row group larger than cache" regime at MB scale —
        /// the same mechanism as fieldmaps-adm4's 1.3 GiB geometry chunks vs the
        /// 256 MiB floor. Run with:
        ///
        /// ```text
        /// cargo test -p tylertoo-core --features remote --lib \
        ///   remote_tests::bench_remote_refetch_ratio -- --ignored --nocapture
        /// ```
        #[test]
        #[ignore = "benchmark: prints the #261 fetch-amplification ratio"]
        fn bench_remote_refetch_ratio() {
            // 3 row groups, each ~1.4 MiB of wide-column pages; floor 256 KiB
            // sits below one row group, so the pre-fix cache thrashed.
            let bytes = multi_row_group_wide_parquet(8192, 3);
            let object_size = bytes.len() as u64;
            let cap_base = 256 * 1024;
            let source =
                super::super::test_memory_source_with_cap(bytes, "bench-wide.parquet", cap_base);

            let reader = source.open().unwrap().with_batch_rows(64).build().unwrap();
            let rows: usize = reader.map(|b| b.unwrap().num_rows()).sum();
            assert_eq!(rows, 8192 * 3);

            let stats = source.fetch_stats().unwrap();
            let ratio = stats.bytes_fetched as f64 / object_size as f64;
            eprintln!(
                "[#261 bench] object={} B  moved={} B  ratio={:.2}×  requests={}  \
                 cap_base={} B  max_rg≈{} B",
                object_size,
                stats.bytes_fetched,
                ratio,
                stats.requests,
                cap_base,
                object_size / 3,
            );
            assert!(
                ratio <= 1.5,
                "single read amplified {ratio:.2}× (expected ≈1× with the \
                 footer-sized cap; a large ratio means #261 regressed)"
            );
        }

        #[test]
        fn out_of_bounds_range_is_an_error() {
            let source = memory_source(tiny_parquet(), "tiny.parquet");
            let InputSource::Remote(ref r) = source else {
                unreachable!()
            };
            let reader = r.reader();
            let size = reader.object_size();
            assert!(reader.get_bytes_range(size - 1, 2).is_err());
        }

        /// Rewrite `bytes`' footer so every column chunk declares
        /// `total_compressed_size = len` (#430 hostile-footer fixture): the
        /// data pages are kept verbatim, only the thrift footer is replaced.
        fn with_hostile_chunk_len(bytes: &[u8], len: i64) -> Vec<u8> {
            with_hostile_chunks(bytes, |b| b.set_total_compressed_size(len))
        }

        /// Rewrite `bytes`' footer so every column chunk declares
        /// `data_page_offset = offset` (and no dictionary page).
        fn with_hostile_chunk_offset(bytes: &[u8], offset: i64) -> Vec<u8> {
            with_hostile_chunks(bytes, |b| {
                b.set_dictionary_page_offset(None)
                    .set_data_page_offset(offset)
            })
        }

        /// Apply `mutate` to every column chunk's metadata builder and
        /// re-serialize the footer behind the original data pages.
        fn with_hostile_chunks(
            bytes: &[u8],
            mutate: impl Fn(
                tylertoo_io::test_support::ColumnChunkMetaDataBuilder,
            ) -> tylertoo_io::test_support::ColumnChunkMetaDataBuilder,
        ) -> Vec<u8> {
            use tylertoo_io::test_support::ParquetMetaDataWriter;

            let md = tylertoo_io::read_parquet_metadata(&Bytes::from(bytes.to_vec())).unwrap();
            let row_groups: Vec<_> = md
                .row_groups()
                .iter()
                .map(|rg| {
                    let cols: Vec<_> = rg
                        .columns()
                        .iter()
                        .map(|c| mutate(c.clone().into_builder()).build().unwrap())
                        .collect();
                    rg.clone()
                        .into_builder()
                        .set_column_metadata(cols)
                        .build()
                        .unwrap()
                })
                .collect();
            let md = Arc::unwrap_or_clone(md)
                .into_builder()
                .set_row_groups(row_groups)
                .build();
            let footer_len =
                u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().unwrap())
                    as usize;
            let data_end = bytes.len() - 8 - footer_len;
            let mut out = bytes[..data_end].to_vec();
            ParquetMetaDataWriter::new(&mut out, &md).finish().unwrap();
            out
        }

        /// #430: a footer whose column chunk range runs past EOF must be a
        /// typed error from open / staging, never a panic (debug overflow)
        /// or a wrapped range (release).
        #[test]
        fn hostile_footer_chunk_past_eof_is_error_not_panic() {
            let good = tiny_parquet();
            // -1: parquet's `byte_range()` asserts on a negative length, so
            // the check must reject it before calling that.
            for len in [good.len() as i64 * 4, i64::MAX, -1] {
                let bytes = with_hostile_chunk_len(&good, len);
                let source = memory_source(bytes, "hostile.parquet");
                let err = match source.open() {
                    Ok(builder) => match builder.build() {
                        Ok(reader) => reader
                            .map(|b| b.map(|_| ()))
                            .collect::<Result<Vec<()>, _>>()
                            .err()
                            .map(InputError::from)
                            .expect("hostile chunk range must not read cleanly"),
                        Err(e) => InputError::from(e),
                    },
                    Err(e) => e,
                };
                let msg = err.to_string();
                assert!(
                    msg.contains("beyond object size") || msg.contains("EOF"),
                    "len={len}: expected an EOF-style error, got: {msg}"
                );
                assert!(
                    source.stage_row_groups(None).is_err(),
                    "len={len}: staging must reject the hostile range"
                );
            }
        }

        /// #430: a negative `data_page_offset` must be a typed error, not
        /// parquet's `byte_range()` assertion panic.
        #[test]
        fn hostile_footer_negative_page_offset_is_error_not_panic() {
            let bytes = with_hostile_chunk_offset(&tiny_parquet(), -1);
            let source = memory_source(bytes, "hostile.parquet");
            let err = source.open().expect_err("negative offset must not open");
            let msg = err.to_string();
            assert!(
                msg.contains("negative") && msg.contains("EOF"),
                "expected a negative-offset EOF error, got: {msg}"
            );
            assert!(source.stage_row_groups(None).is_err());
        }

        /// Object store double that returns `drop` fewer bytes than
        /// requested for any range inside the data region (a truncated
        /// download); footer reads are served intact so the reader gets
        /// as far as slicing column chunks.
        #[derive(Debug)]
        struct TruncatingStore {
            inner: object_store::memory::InMemory,
            data_end: u64,
            drop: usize,
        }

        impl std::fmt::Display for TruncatingStore {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "TruncatingStore({})", self.inner)
            }
        }

        #[async_trait::async_trait]
        impl ObjectStore for TruncatingStore {
            async fn put_opts(
                &self,
                location: &ObjectPath,
                payload: object_store::PutPayload,
                opts: object_store::PutOptions,
            ) -> object_store::Result<object_store::PutResult> {
                self.inner.put_opts(location, payload, opts).await
            }

            async fn put_multipart_opts(
                &self,
                location: &ObjectPath,
                opts: object_store::PutMultipartOptions,
            ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
                self.inner.put_multipart_opts(location, opts).await
            }

            async fn get_opts(
                &self,
                location: &ObjectPath,
                options: object_store::GetOptions,
            ) -> object_store::Result<object_store::GetResult> {
                use futures_util::StreamExt;
                let result = self.inner.get_opts(location, options).await?;
                if result.range.start >= self.data_end {
                    return Ok(result);
                }
                let meta = result.meta.clone();
                let attributes = result.attributes.clone();
                let full = result.range.clone();
                let bytes = result.bytes().await?;
                let keep = bytes.len().saturating_sub(self.drop);
                let short = bytes.slice(..keep);
                Ok(object_store::GetResult {
                    payload: object_store::GetResultPayload::Stream(
                        futures_util::stream::once(futures_util::future::ready(Ok(short))).boxed(),
                    ),
                    meta,
                    range: full.start..full.start + keep as u64,
                    attributes,
                    extensions: Default::default(),
                })
            }

            fn delete_stream(
                &self,
                locations: futures_util::stream::BoxStream<
                    'static,
                    object_store::Result<ObjectPath>,
                >,
            ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectPath>>
            {
                self.inner.delete_stream(locations)
            }

            fn list(
                &self,
                prefix: Option<&ObjectPath>,
            ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectMeta>>
            {
                self.inner.list(prefix)
            }

            async fn list_with_delimiter(
                &self,
                prefix: Option<&ObjectPath>,
            ) -> object_store::Result<object_store::ListResult> {
                self.inner.list_with_delimiter(prefix).await
            }

            async fn copy_opts(
                &self,
                from: &ObjectPath,
                to: &ObjectPath,
                options: object_store::CopyOptions,
            ) -> object_store::Result<()> {
                self.inner.copy_opts(from, to, options).await
            }
        }

        fn truncating_source(bytes: Vec<u8>, drop: usize) -> InputSource {
            use object_store::ObjectStoreExt;

            let footer_len =
                u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().unwrap())
                    as u64;
            let data_end = bytes.len() as u64 - 8 - footer_len;
            let inner = object_store::memory::InMemory::new();
            let location = ObjectPath::from("short.parquet");
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(inner.put(&location, bytes.into())).unwrap();
            let store: Arc<dyn ObjectStore> = Arc::new(TruncatingStore {
                inner,
                data_end,
                drop,
            });
            InputSource::Remote(
                RemoteSource::from_store(store, location, "memory://short.parquet".into()).unwrap(),
            )
        }

        /// #430: a store that returns fewer bytes than requested for a
        /// column chunk (truncated download) must surface as a read error,
        /// not a `Bytes::slice` panic.
        #[test]
        fn truncated_chunk_read_is_error_not_panic() {
            let source = truncating_source(tiny_parquet(), 3);
            let outcome = source.open().and_then(|b| {
                b.build()
                    .map_err(InputError::from)?
                    .map(|batch| batch.map(|_| ()))
                    .collect::<Result<Vec<()>, _>>()
                    .map_err(InputError::from)
            });
            assert!(outcome.is_err(), "short read must be an error");
        }

        /// #430 sequential path: `get_read` inside a truncated chunk must
        /// also fail cleanly.
        #[test]
        fn truncated_chunk_sequential_read_is_error_not_panic() {
            use std::io::Read;
            use tylertoo_io::ChunkReader;

            let bytes = tiny_parquet();
            let source = truncating_source(bytes, 3);
            // Populate the chunk map, then read past what the store returns.
            let builder = source.open().unwrap();
            let (start, len) = builder.metadata().row_group(0).column(0).byte_range();
            let chunk = start..start + len;
            let InputSource::Remote(ref r) = source else {
                unreachable!()
            };
            let reader = InputReader::Remote(r.reader());
            let mut out = Vec::new();
            let res = reader
                .get_read(chunk.end - 1)
                .and_then(|mut rd| rd.read_to_end(&mut out).map_err(ParquetError::from));
            assert!(
                res.is_err(),
                "sequential read past a short chunk must error"
            );
        }

        /// #430: a page read whose declared range exceeds the bytes the
        /// store returned must be an EOF error, not a slice panic.
        #[test]
        fn truncated_chunk_get_bytes_is_eof_error() {
            let source = truncating_source(tiny_parquet(), 3);
            let builder = source.open().unwrap();
            let (start, len) = builder.metadata().row_group(0).column(0).byte_range();
            let InputSource::Remote(ref r) = source else {
                unreachable!()
            };
            let reader = r.reader();
            let err = reader
                .get_bytes_range(start, len as usize)
                .expect_err("short chunk must not be served");
            assert!(
                matches!(err, ParquetError::EOF(_)),
                "expected EOF, got {err:?}"
            );
        }
    }
}
