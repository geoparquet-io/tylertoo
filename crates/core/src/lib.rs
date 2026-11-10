#![recursion_limit = "256"]
// #779: clippy.toml's disallowed-methods name the single owner of parquet
// and IPC reads and of output-file creation. `#[cfg(test)]` code reads and
// writes its own trusted fixtures, so the lint is off in the test build
// only; production code is still linted by the non-test build that
// `cargo clippy --all-targets` also checks.
#![cfg_attr(
    test,
    expect(
        clippy::disallowed_methods,
        reason = "#779: tests construct trusted fixtures; production is checked by the non-test build"
    )
)] // #779: test build only
//! GeoParquet to PMTiles: the library behind the `tylertoo` CLI and Python
//! package.
//!
//! The supported API is the set of items at this crate root:
//!
//! | Task | Entry point | Options | Report | Error |
//! |---|---|---|---|---|
//! | GeoParquet → PMTiles in one call | [`convert_to_pmtiles`] | [`TilesOptions`] | [`TilesOutcome`] | [`TilesError`] |
//! | GeoParquet → overview GeoParquet | [`convert_to_overviews`] | [`ConvertOptions`] | [`ConvertReport`] | [`ConvertError`] |
//! | overview GeoParquet → PMTiles | [`export_pmtiles`] | [`ExportOptions`] | [`ExportReport`] | [`ExportError`] |
//! | Validate an overview file | [`validate_file`] | | [`ValidationReport`] | [`CheckError`] |
//! | Build or merge zoom bands | [`build_pyramid`], [`merge_bands`] | [`PyramidOptions`], [`BandMergeOptions`] | [`PyramidReport`] | [`PyramidError`] |
//! | PMTiles → GeoParquet | [`decode_pmtiles`] | [`DecodeOptions`] | [`DecodeReport`] | [`DecodeError`] |
//!
//! Options start from `Default` and change through `with_*` setters (or
//! their public fields); each facade function checks them before any input
//! is read. [`Classify::kind`] says what any error means to the caller
//! ([`ErrorKind`]). Options, reports, errors and kinds are
//! `#[non_exhaustive]`, so a minor release can add a field, variant or
//! setter.
//!
//! ```
//! use std::path::Path;
//! use tylertoo_core::{
//!     convert_to_pmtiles, Classify, ConvertOptions, ErrorKind, LevelPlan, TilesInput, TilesOptions,
//! };
//!
//! let options = TilesOptions::default().with_convert(
//!     ConvertOptions::default()
//!         .with_levels(LevelPlan::ZoomRange { min_zoom: 0, max_zoom: 12 })
//!         .with_read_batch_size(0),
//! );
//! // Refused before the (missing) input is read.
//! let input = TilesInput::Path("missing.parquet".into());
//! let err = convert_to_pmtiles(&input, Path::new("out.pmtiles"), &options).unwrap_err();
//! assert_eq!(err.kind(), ErrorKind::Usage);
//! ```
//!
//! A struct literal does not compile outside this crate, so adding a field
//! is not a breaking change:
//!
//! ```compile_fail
//! let options = tylertoo_core::DecodeOptions { min_zoom: None, max_zoom: None, layer: None };
//! ```
//!
//! No signature here names an `arrow`, `parquet`, `geo-types`, `geoarrow`
//! or `object_store` type, so their major releases do not break this API.
//! The `internal` module holds everything else for the CLI, the Python
//! bindings, benches, fuzz targets and tests. It is hidden from these
//! docs, and has no stability guarantee.
//!
//! # Memory Profiling
//!
//! Build with `--features dhat-heap` to enable heap profiling with dhat.
//! When enabled, the program will write `dhat-heap.json` on exit which can
//! be analyzed at <https://nnethercote.github.io/dh_view/dh_view.html>

// dhat global allocator - must be at crate root
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use thiserror::Error;

/// `with_*` setters for an options struct (#436), one per public field.
///
/// The options structs are `#[non_exhaustive]`, so callers outside this
/// crate cannot build them with a struct literal; they start from
/// `Default` and chain these setters. The `const` destructures the struct
/// with every listed field, so a field added without a setter fails to
/// compile.
macro_rules! with_setters {
    ($ty:ident { $($setter:ident => $field:ident: $fty:ty,)* }) => {
        impl $ty {
            $(
                #[doc = concat!("Set [`", stringify!($field), "`](Self::", stringify!($field), ").")]
                #[must_use]
                pub fn $setter(mut self, value: $fty) -> Self {
                    self.$field = value;
                    self
                }
            )*
        }
        const _: fn($ty) = |options| {
            let $ty { $($field: _,)* } = options;
        };
    };
}

// Include the protobuf-generated code
mod vector_tile {
    include!(concat!(env!("OUT_DIR"), "/vector_tile.rs"));
}

mod archive_index;
mod atomic_output;
mod batch_processor;
mod clip;
mod compression;
mod covering;
mod decode;
mod dedup;
mod diagnostics;
mod error;
mod fs_probe;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
mod fuzz;
mod input;
mod input_set;
mod ioverlay_clip;
mod merge;
mod mvt;
mod overview;
mod pmtiles_metadata;
mod pmtiles_writer;
mod pyramid;
mod quality;
mod resolved_schema;
mod shard;
mod stats;
mod sutherland_hodgman;
#[cfg(test)]
pub(crate) mod test_log;
mod tile;
mod tile_format;
#[cfg(test)]
mod valve_counter;
mod wkb;
pub(crate) mod wkb_column;

// The supported API (#437): the product entry points, their options,
// reports and errors, and the field types needed to build, read and match
// them. Everything else is `internal`.

// Convert: GeoParquet -> overview GeoParquet.
pub use overview::convert::{
    convert_to_overviews, write_convert_plan, ClassRanking, ClassRankingError, ConvertError,
    ConvertOptions, ConvertReport, GeneralizationOverrides, InvalidOption, LevelPlan, LevelReport,
    OutOfRangeExemplar, PlanLevelReport, PlanReport, RepresentationBand, SkippedLevelReport,
};
pub use overview::{
    assign::{AssignConfig, BudgetKinds, DensityBudgetConfig, FeatureKind, SortDirection},
    cluster::{AccumulateOp, AccumulateSpec},
    filter::FilterError,
    ladder::{EntryZoomKind, EntryZoomSpec},
    level::{Crs, MemoryProfile, Mode, OverviewValidationError, RankingProvenance},
    properties::{PropertySelection, PropertySelectionError},
    reader::ReaderError,
    simplify::{CollapseMode, Representation, SimplifyOptions},
    writer::{RowGroupSizePolicy, WriterError},
};
// Export: overview GeoParquet -> PMTiles.
pub use overview::export::{
    export_pmtiles, ExportError, ExportOptions, ExportReport, FeatureIdColumnProblem, FeatureOrder,
    InvalidFeatureIdReason, SizeValve, SkippedPropertyColumn, VariableDepthReport, ZoomReport,
};
// Check: validate an overview file against the spec.
pub use overview::check::{validate_file, CheckError, CheckResult, ValidationReport};
// Tiles: the one-shot GeoParquet -> PMTiles facade, and its sharded builds.
pub use overview::tiles::{
    convert_to_pmtiles, write_plan, EmptyArchive, IntermediateOverview, ShardBinding, TilesError,
    TilesInput, TilesOptions, TilesOutcome, TilesReport, TilesShard,
};
pub use shard::{ShardError, ShardInput, ShardPlan, ShardRange, ShardRole, TileRange};
// Pyramid: build or merge zoom bands into one archive.
pub use pyramid::{
    build_pyramid, merge_bands, merge_bands_with_options, Band, BandMergeOptions, BandSpecError,
    PyramidError, PyramidOptions, PyramidReport,
};
// Decode: PMTiles -> GeoParquet.
pub use decode::{decode_pmtiles, DecodeError, DecodeOptions, DecodeReport};
// Shared: option validation, diagnostics, errors and their classifier.
pub use archive_index::ArchiveError;
pub use atomic_output::OutputNotWritable;
pub use compression::Compression;
pub use diagnostics::{
    AccountingError, CoveringWarning, CrsWarning, Diagnostic, PolygonRepairKind, RangeContext,
    SourceLoss, SourceLossCause, SourceLosses,
};
pub use error::{Classify, ErrorKind};
pub use input::{FetchStats, InputError};
pub use overview::config::{ConfigError, OutputKind};
// The facade validates its own options; the frontends call `validate()`
// early for parity, so the proof type stays reachable but unstable.
#[doc(hidden)]
pub use overview::config::ValidatedConfig;
pub use pmtiles_metadata::TilesetMetadata;
pub use pmtiles_writer::{TileType, WriteError};
pub use resolved_schema::GeoVersion;
pub use tile_format::{MltProfile, TileFormat};

#[doc(hidden)]
/// Implementation detail of the tylertoo CLI and Python bindings; no
/// stability guarantee.
///
/// Everything here (codecs, readers, writers, geometry kernels, the CLI
/// argument layer) exists so the `tylertoo` CLI, the Python bindings, and
/// this crate's benches, fuzz targets and integration tests can reach it.
/// Any release may change or remove any item without a semver bump. The
/// supported API is the crate root.
pub mod internal {
    /// The MLT codec, for tests that decode MLT archives independently.
    #[cfg(feature = "mlt")]
    pub use mlt_core;
    pub mod archive_index {
        pub use crate::archive_index::*;
    }
    pub mod atomic_output {
        pub use crate::atomic_output::*;
    }
    pub mod batch_processor {
        pub use crate::batch_processor::*;
    }
    pub mod clip {
        pub use crate::clip::*;
    }
    pub mod compression {
        pub use crate::compression::*;
    }
    pub mod covering {
        pub use crate::covering::*;
    }
    pub mod decode {
        pub use crate::decode::*;
    }
    pub mod dedup {
        pub use crate::dedup::*;
    }
    pub mod diagnostics {
        pub use crate::diagnostics::*;
    }
    pub mod error {
        pub use crate::error::*;
    }
    pub mod fs_probe {
        pub use crate::fs_probe::*;
    }
    pub mod input {
        pub use crate::input::*;
    }
    pub mod input_set {
        pub use crate::input_set::*;
    }
    pub mod ioverlay_clip {
        pub use crate::ioverlay_clip::*;
    }
    pub mod merge {
        pub use crate::merge::*;
    }
    pub mod mvt {
        pub use crate::mvt::*;
    }
    pub mod overview {
        pub use crate::overview::*;
    }
    pub mod pmtiles_writer {
        pub use crate::pmtiles_writer::*;
    }
    pub mod pyramid {
        pub use crate::pyramid::*;
    }
    pub mod quality {
        pub use crate::quality::*;
    }
    pub mod resolved_schema {
        pub use crate::resolved_schema::*;
    }
    pub mod shard {
        pub use crate::shard::*;
    }
    pub mod stats {
        pub use crate::stats::*;
    }
    pub mod sutherland_hodgman {
        pub use crate::sutherland_hodgman::*;
    }
    pub mod tile {
        pub use crate::tile::*;
    }
    pub mod vector_tile {
        pub use crate::vector_tile::*;
    }
    pub mod wkb {
        pub use crate::wkb::*;
    }
    #[cfg(feature = "fuzzing")]
    pub mod fuzz {
        pub use crate::fuzz::*;
    }
    pub use crate::pmtiles_writer::{StreamingPmtilesWriter, StreamingWriteStats};
    pub use crate::quality::{extract_crs, validate_wgs84, CrsInfo};
}

/// Errors that can occur while reading GeoParquet or reading and writing
/// PMTiles.
///
/// `#[non_exhaustive]`: new failure modes are added without a breaking
/// change; match with a wildcard arm. [`error::Classify::kind`] says what a
/// failure means to the caller.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Source loss accounting failed.
    #[error(transparent)]
    Accounting(#[from] crate::diagnostics::AccountingError),
    /// A GeoParquet input could not be read; the payload says why.
    #[error("Failed to read GeoParquet file: {0}")]
    GeoParquetRead(GeoParquetReadError),

    /// Writing a PMTiles archive failed (#438).
    #[error("Failed to write PMTiles: {0}")]
    Write(pmtiles_writer::WriteError),

    /// A PMTiles archive could not be read (#438).
    #[error("Failed to read PMTiles: {0}")]
    Archive(archive_index::ArchiveError),

    /// `error`, at `path`: the archive or output it concerns, for a caller
    /// that holds several.
    #[error("{}: {error}", path.display())]
    #[non_exhaustive]
    At {
        /// The archive or output.
        path: std::path::PathBuf,
        /// What went wrong there.
        error: Box<Error>,
    },

    /// A refused bounds spec, or a refused set of archives to merge.
    #[error("Invalid configuration: {0}")]
    InvalidConfig(InvalidConfigError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    /// The output cannot be written (#452: checked before any work).
    #[error(transparent)]
    Output(#[from] atomic_output::OutputNotWritable),
}

// Not `#[from]`: the inner error's message is already in this one's, so it
// is not also a `source()` the CLI would print a second time.
impl From<pmtiles_writer::WriteError> for Error {
    fn from(e: pmtiles_writer::WriteError) -> Self {
        Error::Write(e)
    }
}

impl From<archive_index::ArchiveError> for Error {
    fn from(e: archive_index::ArchiveError) -> Self {
        Error::Archive(e)
    }
}

impl Error {
    /// `error`, attributed to `path` ([`Error::At`]).
    pub(crate) fn at(path: &std::path::Path, error: impl Into<Error>) -> Self {
        Error::At {
            path: path.to_path_buf(),
            error: Box::new(error.into()),
        }
    }
}

impl error::Classify for Error {
    fn kind(&self) -> error::ErrorKind {
        match self {
            Error::InvalidConfig(_) => error::ErrorKind::Usage,
            Error::At { error, .. } => error.kind(),
            _ => error::fallback_kind(self),
        }
    }
}

/// Why a GeoParquet input could not be read: the payload of
/// [`Error::GeoParquetRead`]. A wrapped third-party error is kept as its
/// message.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum GeoParquetReadError {
    /// The input path does not exist.
    #[error("Path does not exist: {}", path.display())]
    #[non_exhaustive]
    PathNotFound {
        /// The path as given.
        path: std::path::PathBuf,
    },
    /// A directory input holds no `.parquet` file.
    #[error("No .parquet files found in directory: {}", path.display())]
    #[non_exhaustive]
    NoParquetFiles {
        /// The directory.
        path: std::path::PathBuf,
    },
    /// A directory input could not be listed.
    #[error("Failed to read directory {}: {error}", path.display())]
    #[non_exhaustive]
    ReadDirectory {
        /// The directory.
        path: std::path::PathBuf,
        /// The listing error.
        error: Box<input::InputError>,
    },
    /// The file could not be opened.
    #[error("Failed to open {}: {error}", path.display())]
    #[non_exhaustive]
    Open {
        /// The file.
        path: std::path::PathBuf,
        /// The open error.
        error: std::io::Error,
    },
    /// The file's Parquet footer could not be read.
    #[error("Failed to read {}: {message}", path.display())]
    #[non_exhaustive]
    Footer {
        /// The file.
        path: std::path::PathBuf,
        /// The reader's message.
        message: String,
    },
    /// The input could not be resolved or its schema read.
    #[error("{0}")]
    Input(Box<input::InputError>),
    /// The footer's Arrow schema could not be decoded.
    #[error("{message}")]
    #[non_exhaustive]
    Schema {
        /// The reader's message.
        message: String,
    },
    /// The geometry column's Arrow type is not a GeoArrow geometry type.
    #[error("Unsupported geometry type: {data_type}")]
    #[non_exhaustive]
    UnsupportedGeometryType {
        /// The column's Arrow data type, as `Debug` prints it.
        data_type: String,
    },
    /// One geometry value is malformed.
    #[error("Invalid geometry at index {index}: {message}")]
    #[non_exhaustive]
    InvalidGeometry {
        /// Row index within the batch or chunk.
        index: usize,
        /// The decoder's message.
        message: String,
    },
    /// A batch or its geometry column could not be decoded.
    #[error("{stage}: {message}")]
    #[non_exhaustive]
    Decode {
        /// What was being decoded (`geometry decode`, `fill seed read`, …).
        stage: &'static str,
        /// The decoder's message.
        message: String,
    },
    /// The `geo` metadata is not valid JSON.
    #[error("Failed to parse geo metadata JSON: {0}")]
    GeoMetadataJson(serde_json::Error),
    /// The `geo` metadata is JSON but not an object.
    #[error(
        "the 'geo' metadata is valid JSON but not an object; rewrite the file's \
         GeoParquet metadata (e.g. re-save it with `gpio convert`)"
    )]
    GeoMetadataNotObject,
    /// The persisted resolved-schema footer is not valid JSON.
    #[error("invalid resolved schema: {0}")]
    ResolvedSchemaJson(serde_json::Error),
    /// The persisted resolved-schema footer disagrees with the file.
    #[error("resolved schema disagrees with the Parquet footer")]
    ResolvedSchemaMismatch,
    /// The resolved schema could not be serialized for the output footer.
    #[error("{0}")]
    ResolvedSchemaEncode(serde_json::Error),
    /// A bbox covering column is not a struct.
    #[error("bbox covering column is not a struct")]
    CoveringNotStruct,
    /// A bbox covering child field is not `Float64`.
    #[error("bbox covering field {field} is not Float64")]
    #[non_exhaustive]
    CoveringFieldNotFloat64 {
        /// The child field.
        field: String,
    },
    /// A bbox covering column is not a struct of `f64` fields.
    #[error("bbox covering {column:?} is not an f64 struct")]
    #[non_exhaustive]
    CoveringNotF64Struct {
        /// The covering column.
        column: String,
    },
    /// The input is not in WGS84.
    #[error(
        "Input file uses CRS {crs}.\n\
         tylertoo requires WGS84 (EPSG:4326) coordinates.\n\n\
         Reproject with geoparquet-io:\n  \
         gpio convert reproject {file} reprojected.parquet -d EPSG:4326"
    )]
    #[non_exhaustive]
    NotWgs84 {
        /// The CRS, as the message names it.
        crs: String,
        /// The input's file name.
        file: String,
    },
    /// An export read returned a row outside its selected row domain
    /// (an internal invariant).
    #[error("export read exceeded its selected row domain")]
    RowDomainExceeded,
    /// `error`, in the input rows `start..end` (#460: a decode error's own
    /// index is chunk-local).
    #[error("rows {start}..{end}: {}", rows_inner(error))]
    #[non_exhaustive]
    AtRows {
        /// First global row of the chunk.
        start: usize,
        /// One past its last row.
        end: usize,
        /// The failure.
        error: Box<Error>,
    },
}

/// What [`Error::InvalidConfig`] refused: a bounds spec, or a set of
/// archives to merge.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum InvalidConfigError {
    /// A `z/x/y` tile spec past zoom 31.
    #[error("Invalid tile '{input}': zoom {zoom} is past the maximum of 31")]
    #[non_exhaustive]
    TileZoomTooHigh {
        /// The spec as given.
        input: String,
        /// Its zoom.
        zoom: u8,
    },
    /// A `z/x/y` tile spec whose `x` or `y` is outside the zoom's tiles.
    #[error("Invalid tile '{input}': x and y must be below {tiles} at zoom {zoom}")]
    #[non_exhaustive]
    TileOutOfRange {
        /// The spec as given.
        input: String,
        /// Its zoom.
        zoom: u8,
        /// Tiles per axis at that zoom.
        tiles: u64,
    },
    /// A bounds spec that is neither `z/x/y` nor four numbers.
    #[error("Invalid bounds format '{input}'. Expected 'z/x/y' or 'xmin,ymin,xmax,ymax'")]
    #[non_exhaustive]
    BoundsFormat {
        /// The spec as given.
        input: String,
    },
    /// One bounds value is not a number.
    #[error("Invalid {name} value '{value}' in bounds")]
    #[non_exhaustive]
    BoundsValue {
        /// `xmin`, `ymin`, `xmax` or `ymax`.
        name: &'static str,
        /// The value as given.
        value: String,
    },
    /// A bounds value is not finite.
    #[error("Invalid bounds '{input}': every value must be a finite number")]
    #[non_exhaustive]
    BoundsNotFinite {
        /// The spec as given.
        input: String,
    },
    /// A bounds minimum is not below its maximum.
    #[error("{axis}min ({min}) must be less than {axis}max ({max})")]
    #[non_exhaustive]
    BoundsInverted {
        /// `x` or `y`.
        axis: &'static str,
        /// The minimum.
        min: f64,
        /// The maximum.
        max: f64,
    },
    /// `merge` was given no archive.
    #[error("merge needs at least one input archive")]
    MergeNoInputs,
    /// Two merge inputs hold different tile types.
    #[error(
        "inputs disagree on tile type: {} holds {first_type:?} but {} holds {other_type:?}; \
         merge copies tile bodies verbatim under one header and cannot describe both, so \
         build every input with the same --tile-format",
        first.display(),
        other.display()
    )]
    #[non_exhaustive]
    MergeTileTypeMismatch {
        /// The first input.
        first: std::path::PathBuf,
        /// Its tile type.
        first_type: TileType,
        /// The input that disagrees.
        other: std::path::PathBuf,
        /// Its tile type.
        other_type: TileType,
    },
    /// Two merge inputs use different tile compressions.
    #[error(
        "inputs disagree on tile compression: {} is {first_compression:?} but {} is \
         {other_compression:?}; merge copies tile bodies verbatim, so a mixed set would \
         mislabel every tile of one of them — re-tile the odd one out",
        first.display(),
        other.display()
    )]
    #[non_exhaustive]
    MergeCompressionMismatch {
        /// The first input.
        first: std::path::PathBuf,
        /// Its tile compression.
        first_compression: Compression,
        /// The input that disagrees.
        other: std::path::PathBuf,
        /// Its tile compression.
        other_compression: Compression,
    },
    /// The merge inputs are not vector tile (MVT or MLT) archives.
    #[error(
        "{}: tile type is {tile_type:?}, but merge writes vector tile (MVT or MLT) archives",
        path.display()
    )]
    #[non_exhaustive]
    MergeNotMvt {
        /// The first input.
        path: std::path::PathBuf,
        /// Its tile type.
        tile_type: TileType,
    },
    /// Two merge inputs (or one, twice) address the same tile id.
    #[error(
        "{} and {} claim overlapping tile ids: both address tile id {tile_id} ({zxy}). merge \
         concatenates disjoint shards — it will not reconcile a tile two inputs both claim, \
         so check how the shards were cut (a shard listed twice, or one left over from an \
         earlier run, is the usual cause). The same archive named twice means its own \
         directory addresses that id more than once, which no valid PMTiles archive does",
        first.display(),
        second.display()
    )]
    #[non_exhaustive]
    MergeOverlap {
        /// The input that claimed the id first.
        first: std::path::PathBuf,
        /// The input that claimed it again.
        second: std::path::PathBuf,
        /// The tile id.
        tile_id: u64,
        /// The id as `z{z}/{x}/{y}`, or `?`.
        zxy: String,
    },
    /// The inputs' publication metadata cannot fit an archive's metadata
    /// section.
    #[error("{message}")]
    #[non_exhaustive]
    MetadataTooLarge {
        /// Which field and by how much.
        message: String,
    },
    /// A pyramid's band convert options are refused.
    #[error("band convert options: {0}")]
    BandConvertOptions(Box<overview::convert::ConvertError>),
    /// `operation` (merge, pyramid) was given a variable-depth archive
    /// (#803), whose missing tiles mean "draw the stored ancestor", which
    /// another archive's tiles there would contradict.
    #[error(
        "{}: is a variable-depth archive ({}), whose missing tiles are drawn by a stored \
         ancestor; {operation} cannot combine it with other tiles. Re-tile it without \
         --generate-variable-depth-tile-pyramid",
        path.display(),
        crate::pmtiles_writer::VARIABLE_DEPTH_KEY
    )]
    #[non_exhaustive]
    VariableDepthArchive {
        /// The variable-depth archive.
        path: std::path::PathBuf,
        /// `merge` or `pyramid`.
        operation: &'static str,
    },
}

impl From<InvalidConfigError> for Error {
    fn from(reason: InvalidConfigError) -> Self {
        Error::InvalidConfig(reason)
    }
}

/// A [`GeoParquetReadError::AtRows`] cause without its own
/// "Failed to read GeoParquet file" prefix, so the sentence is not repeated.
fn rows_inner(error: &Error) -> String {
    match error {
        Error::GeoParquetRead(reason) => reason.to_string(),
        other => other.to_string(),
    }
}

impl GeoParquetReadError {
    /// A [`Self::Decode`] failure of `stage`, as an [`Error`].
    pub(crate) fn decode(stage: &'static str, e: impl std::fmt::Display) -> Error {
        GeoParquetReadError::Decode {
            stage,
            message: e.to_string(),
        }
        .into()
    }
}

impl From<GeoParquetReadError> for Error {
    fn from(reason: GeoParquetReadError) -> Self {
        Error::GeoParquetRead(reason)
    }
}

#[doc(hidden)]
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::error::{Classify, ErrorKind};
    use super::*;

    #[test]
    fn only_configuration_errors_are_classified_as_usage() {
        assert_eq!(
            Error::InvalidConfig(InvalidConfigError::MergeNoInputs).kind(),
            ErrorKind::Usage
        );
        for error in [
            Error::GeoParquetRead(GeoParquetReadError::ResolvedSchemaMismatch),
            archive_index::ArchiveError::Header("invalid header".into()).into(),
            pmtiles_writer::WriteError::Finalized.into(),
            Error::Io(std::io::Error::other("disk full")),
            Error::Accounting(diagnostics::AccountingError::Io(std::io::Error::other(
                "spill failed",
            ))),
        ] {
            assert_eq!(error.kind(), ErrorKind::Runtime, "{error}");
        }
    }

    /// An error attributed to a path keeps its own kind and prefixes the path.
    #[test]
    fn an_attributed_error_keeps_its_kind() {
        let at = Error::at(
            std::path::Path::new("a.pmtiles"),
            Error::InvalidConfig(InvalidConfigError::MergeNoInputs),
        );
        assert_eq!(at.kind(), ErrorKind::Usage);
        assert_eq!(
            at.to_string(),
            "a.pmtiles: Invalid configuration: merge needs at least one input archive"
        );
    }
}
