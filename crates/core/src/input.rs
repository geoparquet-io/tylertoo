//! Input-source abstraction: local files or remote objects (issue #210).
//!
//! The overview converter historically opened its input with
//! [`std::fs::File`]. This module generalizes that to an [`InputSource`]
//! that can also point at an object in remote storage (`s3://`, `https://`,
//! `http://`, `gs://`), served to the existing *synchronous* parquet reader
//! plumbing through the [`parquet::file::reader::ChunkReader`] trait:
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
//!   [`remote::CHUNK_CACHE_MAX_BYTES`]), so a row group larger than the floor
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
//! fail with a clear [`InputError::RemoteDisabled`] error.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::errors::ParquetError;
use parquet::file::reader::{ChunkReader, Length};
use serde::Serialize;

/// Schemes recognized as remote inputs (when the `remote` feature is on).
const REMOTE_SCHEMES: &[&str] = &["s3", "s3a", "http", "https", "gs"];

/// Errors from opening or reading an [`InputSource`].
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// Local I/O error.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// Parquet error (footer parse, reader construction).
    #[error("parquet error: {0}")]
    Parquet(#[from] ParquetError),
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
        "remote input {0:?} requires gpq-tiles-core's `remote` feature (the official \
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
    /// A remote URL that is not a single `.parquet` object (a bucket prefix /
    /// "directory"). Listing remote prefixes lands later in this release.
    #[error(
        "remote prefix input {url:?} is not yet supported (single .parquet object \
         URLs are; remote prefix listing is coming later in this release)"
    )]
    RemotePrefixUnsupported {
        /// The rejected URL.
        url: String,
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
    pub fn from_path(path: &Path) -> Result<Self, InputError> {
        let Some(s) = path.to_str() else {
            // Non-UTF-8 paths cannot be URLs; treat as local.
            return Ok(InputSource::Local(path.to_path_buf()));
        };
        Self::from_str_input(s)
    }

    /// [`InputSource::from_path`] for string inputs.
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
    pub fn open(&self) -> Result<ParquetRecordBatchReaderBuilder<InputReader>, InputError> {
        match self {
            InputSource::Local(p) => {
                let file = File::open(p)?;
                Ok(ParquetRecordBatchReaderBuilder::try_new(
                    InputReader::Local(file),
                )?)
            }
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
    #[cfg_attr(not(feature = "remote"), allow(unused_variables))]
    pub fn set_spill_dir(&self, dir: Option<&Path>) {
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
}

/// Sum of the compressed byte sizes of the selected input row groups — the
/// bytes a remote convert will touch, and therefore the projected size of
/// the disk spill (#219): every touched chunk is staged locally exactly
/// once, so the spill grows to ≈ this number (issue #272 free-space
/// preflight). `None` selects every row group (a full-file read); indices
/// out of range are ignored. Single-file shape on purpose (metadata +
/// selection → bytes): a multi-partition source sums it across parts.
pub fn selected_compressed_bytes(
    metadata: &parquet::file::metadata::ParquetMetaData,
    selected_row_groups: Option<&[usize]>,
) -> u64 {
    let group_bytes = |i: usize| -> u64 {
        metadata
            .row_groups()
            .get(i)
            .map_or(0, |rg| rg.compressed_size().max(0) as u64)
    };
    match selected_row_groups {
        None => (0..metadata.num_row_groups()).map(group_bytes).sum(),
        Some(selected) => selected.iter().map(|&i| group_bytes(i)).sum(),
    }
}

/// Whether `scheme` is one of the recognized remote URL schemes.
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
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};

    use bytes::Bytes;
    use object_store::aws::{AmazonS3Builder, AwsCredential};
    use object_store::gcp::GoogleCloudStorageBuilder;
    use object_store::http::HttpBuilder;
    use object_store::path::Path as ObjectPath;
    use object_store::{ClientOptions, CredentialProvider, ObjectStore, ObjectStoreScheme};
    use parquet::arrow::arrow_reader::{
        ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder,
    };
    use parquet::errors::ParquetError;
    use url::Url;

    use super::{FetchStats, InputError, InputReader};

    /// Readahead for the *sequential* read path
    /// ([`parquet::file::reader::ChunkReader::get_read`]). The page reader
    /// uses `get_read` only to thrift-decode page *headers* (tens of bytes;
    /// page data goes through exact-range `get_bytes`), so keep this small —
    /// and always clamp it to the surrounding column chunk (see
    /// [`SharedState::clamp_to_chunk`]) so a header read near a chunk
    /// boundary can never pull bytes from a bbox-pruned row group.
    const SEQUENTIAL_CHUNK: u64 = 8 * 1024;

    /// Shared tokio runtime driving object_store's async I/O from our
    /// synchronous reader plumbing. Two worker threads: requests are issued
    /// one at a time per reader, so this only needs to run the HTTP client.
    fn runtime() -> &'static tokio::runtime::Runtime {
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
        size: u64,
        shared: Arc<SharedState>,
        metadata: OnceLock<ArrowReaderMetadata>,
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

    impl RemoteSource {
        /// Connect to `url`, resolving the backing store from the scheme
        /// and HEAD-ing the object for its size.
        pub(crate) fn connect(url_str: &str) -> Result<Self, InputError> {
            let url = Url::parse(url_str)
                .map_err(|e| InputError::RemoteConfig(format!("invalid URL {url_str:?}: {e}")))?;
            let (scheme, location) = ObjectStoreScheme::parse(&url).map_err(|e| {
                InputError::RemoteConfig(format!("cannot interpret URL {url_str:?}: {e}"))
            })?;
            let store: Arc<dyn ObjectStore> = match scheme {
                ObjectStoreScheme::AmazonS3 => build_s3(&url)?,
                ObjectStoreScheme::GoogleCloudStorage => Arc::new(
                    GoogleCloudStorageBuilder::from_env()
                        .with_url(url_str)
                        .build()
                        .map_err(|e| store_error(url_str, e))?,
                ),
                ObjectStoreScheme::Http => {
                    let origin = &url[..url::Position::BeforePath];
                    Arc::new(
                        HttpBuilder::new()
                            .with_url(origin)
                            .with_client_options(ClientOptions::new().with_allow_http(true))
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
            Ok(Self {
                url,
                store,
                location,
                size: head.size,
                shared: Arc::new(SharedState::default()),
                metadata: OnceLock::new(),
                cache: Arc::new(Mutex::new(ChunkCache::new(cap_base))),
                spill: Arc::new(Mutex::new(DiskSpill::default())),
                cap_base,
            })
        }

        /// The input URL.
        pub fn url(&self) -> &str {
            &self.url
        }

        /// Open a parquet reader builder; the parsed footer is cached across
        /// opens so multi-pass pipelines fetch it once.
        pub(crate) fn open_builder(
            &self,
        ) -> Result<ParquetRecordBatchReaderBuilder<InputReader>, InputError> {
            let reader = InputReader::Remote(self.reader());
            let metadata = match self.metadata.get() {
                Some(md) => md.clone(),
                None => {
                    let md = ArrowReaderMetadata::load(&reader, ArrowReaderOptions::new())?;
                    // A concurrent open may have won the race; either copy
                    // is equivalent.
                    let _ = self.metadata.set(md.clone());
                    md
                }
            };
            // Column-chunk byte ranges (sorted) for readahead clamping.
            self.shared.chunk_ranges.get_or_init(|| {
                let mut ranges: Vec<Range<u64>> = metadata
                    .metadata()
                    .row_groups()
                    .iter()
                    .flat_map(|rg| {
                        rg.columns().iter().map(|col| {
                            let (start, len) = col.byte_range();
                            start..start + len
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
                .map(|rg| {
                    rg.columns()
                        .iter()
                        .map(|col| col.compressed_size().max(0) as u64)
                        .sum::<u64>()
                })
                .max()
                .unwrap_or(0);
            {
                let mut cache = self.cache.lock().expect("chunk cache lock");
                cache.cap = self.cap_base.max(max_row_group);
            }
            Ok(ParquetRecordBatchReaderBuilder::new_with_metadata(
                reader, metadata,
            ))
        }

        /// A cheap handle for issuing counted range requests.
        pub(crate) fn reader(&self) -> RemoteReader {
            RemoteReader {
                store: Arc::clone(&self.store),
                location: self.location.clone(),
                size: self.size,
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
            self.spill.lock().expect("spill lock").dir = dir.map(Path::to_path_buf);
        }

        /// Snapshot of the fetch counters.
        pub fn fetch_stats(&self) -> FetchStats {
            FetchStats {
                requests: self.shared.requests.load(Ordering::Relaxed),
                bytes_fetched: self.shared.bytes.load(Ordering::Relaxed),
                object_size: self.size,
            }
        }

        /// The byte ranges fetched so far, in request order.
        pub fn fetched_ranges(&self) -> Vec<Range<u64>> {
            self.shared.ranges.lock().expect("ranges lock").clone()
        }

        /// Drop the in-memory (L1) chunk cache. The disk spill (L2) and the
        /// cached footer are KEPT: a later pass re-reads spilled chunks from
        /// local disk, never the network. Multi-partition streams call this
        /// on part transitions so resident memory stays O(one part's row
        /// group) instead of O(parts × cap) (v0.7 multi-partition input).
        pub fn release_read_cache(&self) {
            let mut cache = self.cache.lock().expect("chunk cache lock");
            cache.entries.clear();
            cache.order.clear();
            cache.total = 0;
        }
    }

    /// Map an object-store error, attaching the input URL.
    fn store_error(url: &str, source: object_store::Error) -> InputError {
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

        Ok(Arc::new(builder.build().map_err(|e| {
            InputError::RemoteConfig(format!("cannot configure S3 store for {url}: {e}"))
        })?))
    }

    /// Bridges the AWS SDK credential chain (profiles, SSO, IMDS, ...) into
    /// object_store's credential provider, re-resolving on each request so
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
        size: u64,
        shared: Arc<SharedState>,
        cache: Arc<Mutex<ChunkCache>>,
        spill: Arc<Mutex<DiskSpill>>,
    }

    impl RemoteReader {
        /// Total object size (the [`parquet::file::reader::Length`] answer).
        pub(crate) fn object_size(&self) -> u64 {
            self.size
        }

        /// One counted range GET.
        fn fetch(&self, range: Range<u64>) -> Result<Bytes, ParquetError> {
            let bytes = runtime()
                .block_on(self.store.get_range(&self.location, range.clone()))
                .map_err(|e| ParquetError::External(Box::new(e)))?;
            self.shared.requests.fetch_add(1, Ordering::Relaxed);
            self.shared
                .bytes
                .fetch_add(bytes.len() as u64, Ordering::Relaxed);
            self.shared.ranges.lock().expect("ranges lock").push(range);
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
                let cache = self.cache.lock().expect("chunk cache lock");
                if let Some((_, data)) = cache.entries.get(&chunk.start) {
                    return Ok(data.clone());
                }
            }
            // L2: local disk spill. A hit avoids re-fetching over the network on
            // a later pass; re-warm L1 so same-pass touches stay in memory.
            if let Some(data) = self.spill.lock().expect("spill lock").get(chunk.start) {
                self.cache_insert(chunk, &data);
                return Ok(data);
            }
            // L3: network. Fetch once, then spill the bytes for later passes.
            let data = self.fetch(chunk.clone())?;
            self.spill
                .lock()
                .expect("spill lock")
                .put(chunk.start, &data);
            self.cache_insert(chunk, &data);
            Ok(data)
        }

        /// Insert a chunk into the in-memory L1 cache, evicting in insertion
        /// order until the working-set budget (`cap`, sized to the largest row
        /// group in [`RemoteSource::open_builder`]) is respected.
        fn cache_insert(&self, chunk: &Range<u64>, data: &Bytes) {
            let mut cache = self.cache.lock().expect("chunk cache lock");
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

        /// Exact-range read for [`parquet::file::reader::ChunkReader::get_bytes`].
        pub(crate) fn get_bytes_range(
            &self,
            start: u64,
            length: usize,
        ) -> Result<Bytes, ParquetError> {
            let end = start
                .checked_add(length as u64)
                .filter(|end| *end <= self.size)
                .ok_or_else(|| {
                    ParquetError::EOF(format!(
                        "range {start}..{} beyond object size {} for {}",
                        start as u128 + length as u128,
                        self.size,
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
                let offset = (start - chunk.start) as usize;
                return Ok(data.slice(offset..offset + length));
            }
            self.fetch(start..end)
        }

        /// Chunked sequential reader for
        /// [`parquet::file::reader::ChunkReader::get_read`] (not used by the
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
    pub struct SequentialRemoteRead {
        reader: RemoteReader,
        pos: u64,
        buf: Bytes,
        buf_offset: usize,
    }

    impl std::io::Read for SequentialRemoteRead {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            if self.buf_offset >= self.buf.len() {
                let remaining = self.reader.size.saturating_sub(self.pos);
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
                    let offset = (self.pos - chunk.start) as usize;
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
                    "/nonexistent/gpq-tiles-spill-dir-272",
                )),
                ..DiskSpill::default()
            };
            spill.put(0, &Bytes::from_static(b"hello spill"));
            assert!(
                spill.get(0).is_none(),
                "create failure must disable the spill"
            );
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
    use object_store::ObjectStore;
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
    use object_store::ObjectStore;
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

    #[cfg(feature = "remote")]
    mod remote_tests {
        use super::super::*;

        use super::super::test_memory_source as memory_source;

        /// Minimal single-column parquet bytes for reader plumbing tests.
        fn tiny_parquet() -> Vec<u8> {
            use arrow_array::{Int64Array, RecordBatch};
            use parquet::arrow::ArrowWriter;
            use std::sync::Arc as SArc;

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
            use parquet::file::reader::ChunkReader;
            use std::io::Read;

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
            const URL: &str = "https://github.com/geoparquet-io/gpq-tiles/releases/download/fixtures-v1/fieldmaps-boundaries.parquet";
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
            use parquet::file::properties::WriterProperties;
            use std::sync::Arc as SArc;

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
            let mut w = parquet::arrow::ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))
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

            let reader = source.open().unwrap().with_batch_size(64).build().unwrap();
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
                let reader = source.open().unwrap().with_batch_size(64).build().unwrap();
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
            source.set_spill_dir(Some(Path::new("/nonexistent/gpq-tiles-spill-dir-272")));

            for _ in 0..3 {
                let reader = source.open().unwrap().with_batch_size(64).build().unwrap();
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
                let reader = source.open().unwrap().with_batch_size(64).build().unwrap();
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
        fn multi_row_group_wide_parquet(rows_per_group: usize, groups: usize) -> Vec<u8> {
            use arrow_array::{Int64Array, RecordBatch, StringArray};
            use parquet::file::properties::WriterProperties;
            use std::sync::Arc as SArc;

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
            let mut w = parquet::arrow::ArrowWriter::try_new(&mut buf, batch.schema(), Some(props))
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
        /// cargo test -p gpq-tiles-core --features remote --lib \
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

            let reader = source.open().unwrap().with_batch_size(64).build().unwrap();
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
    }
}
