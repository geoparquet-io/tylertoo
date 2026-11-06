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
#[cfg(test)]
mod valve_counter;
mod wkb;
pub(crate) mod wkb_column;

// The supported API (#437): the product entry points, their options,
// reports and errors, and the field types needed to build, read and match
// them. Everything else is `internal`.

// Convert: GeoParquet -> overview GeoParquet.
pub use overview::convert::{
    convert_to_overviews, write_convert_plan, ClassRanking, ConvertError, ConvertOptions,
    ConvertReport, GeneralizationOverrides, LevelPlan, LevelReport, OutOfRangeExemplar,
    PlanLevelReport, PlanReport, RepresentationBand, SkippedLevelReport,
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
    InvalidFeatureIdReason, SizeValve, SkippedPropertyColumn, ZoomReport,
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
    build_pyramid, merge_bands, merge_bands_with_options, Band, BandMergeOptions, PyramidError,
    PyramidOptions, PyramidReport,
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
    #[error("Failed to read GeoParquet file: {0}")]
    GeoParquetRead(String),

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

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

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
            _ => error::ErrorKind::Runtime,
        }
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
            Error::InvalidConfig("invalid band options".into()).kind(),
            ErrorKind::Usage
        );
        for error in [
            Error::GeoParquetRead("invalid footer".into()),
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
            Error::InvalidConfig("x".into()),
        );
        assert_eq!(at.kind(), ErrorKind::Usage);
        assert_eq!(at.to_string(), "a.pmtiles: Invalid configuration: x");
    }
}
