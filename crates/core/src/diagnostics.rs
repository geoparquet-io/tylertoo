//! Per-run diagnostics shared by decoding, simplification and encoding.
//!
//! A sink belongs to one operation. Workers borrow it; reports take a sorted,
//! aggregated snapshot. No warning or count survives into another operation.
mod accounting;

pub(crate) const MAX_LOSS_REPORT_BYTES: usize = 64 << 20;
use std::fmt;
use std::sync::Mutex;

use crate::overview::convert::{crs_coordinate_range, reprojection_advice, OutOfRangeExemplar};
use crate::overview::level::Crs;
use crate::tile::WEBMERC_HALF_M;
use serde::{Deserialize, Serialize};

/// The integer polygon rule a repair corrected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolygonRepairKind {
    CollapsedRing,
    RingSelfTouch,
    RingsCross,
    HoleOutsideShell,
    NestedHoles,
    DisconnectedInterior,
    PartsOverlap,
    NestedShells,
}

/// Coordinate-system facts needed to explain a scan diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RangeContext {
    /// Usable source rows scanned.
    pub total: usize,
    /// CRS declared by the input.
    pub crs: Crs,
    /// Largest coordinate magnitude among out-of-range rows.
    pub max_abs: f64,
}

/// A counted event or warning. The serialized tag is stable across frontends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Diagnostic {
    /// Facts from the whole saved plan, not counts from this shard's scan.
    SavedPlan {
        count: usize,
        diagnostics: Vec<Diagnostic>,
    },
    /// Advice or an I/O warning from the shared pipeline. Kept per run and
    /// deduplicated by its complete message; never persisted in a plan.
    Advisory {
        count: usize,
        message: String,
    },
    OutOfRange {
        context: Option<RangeContext>,
        count: usize,
        exemplars: Vec<crate::overview::convert::OutOfRangeExemplar>,
    },
    Unprojectable {
        context: Option<RangeContext>,
        count: usize,
    },
    Wrapped {
        exemplars: Vec<OutOfRangeExemplar>,
        context: Option<RangeContext>,
        count: usize,
    },
    AntimeridianSuspect {
        count: usize,
    },
    EncodeDropped {
        count: usize,
    },
    EncodeQuantized {
        count: usize,
    },
    ClipGuardDropped {
        count: usize,
    },
    NonFiniteGeometry {
        count: usize,
    },
    PolygonRepaired {
        count: usize,
        #[serde(rename = "repair_kind")]
        kind: PolygonRepairKind,
    },
    InvalidAfterEncode {
        count: usize,
    },
    RowGroupAutoscaled {
        count: usize,
        cap: usize,
    },
    InvalidAfterSimplify {
        count: usize,
    },
    FullResolutionFallback {
        count: usize,
    },
    ValidationSkipped {
        count: usize,
    },
    Merged {
        count: usize,
    },
    ValveDropped {
        count: usize,
    },
    PropertySkipped {
        count: usize,
        name: String,
        data_type: String,
    },
    CrsMetadata {
        count: usize,
        reason: CrsWarning,
    },
    CoveringRejected {
        count: usize,
        reason: CoveringWarning,
    },
    UnusableGeometry {
        count: usize,
        total: usize,
    },
    CrsAssumed {
        count: usize,
        input: String,
    },
    EmptyLevels {
        count: usize,
        levels: Vec<usize>,
        gsd_min: f64,
        gsd_max: f64,
        input_features: usize,
        first_written_gsd: f64,
        first_written_zoom: Option<u8>,
    },
    MemoryBudget {
        count: usize,
        machine: Option<u64>,
        limit: u64,
    },
    PyramidSharedTiles {
        count: usize,
    },
    PyramidSkippedTiles {
        count: usize,
    },
    PyramidMissingBounds {
        count: usize,
    },
    PyramidMetadataConflict {
        count: usize,
    },
    PyramidMissingMetadata {
        count: usize,
    },
}

/// Why CRS metadata could only be read best-effort.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(tag = "reason", content = "value", rename_all = "snake_case")]
pub enum CrsWarning {
    #[error("GeoParquet file has no key-value metadata; assuming WGS84")]
    NoKeyValueMetadata,
    #[error("GeoParquet file has no 'geo' metadata; assuming WGS84")]
    NoGeoMetadata,
    #[error("GeoParquet 'geo' metadata has no 'columns'; assuming WGS84")]
    NoColumns,
    #[error("GeoParquet 'geo' metadata missing column '{0}'; assuming WGS84")]
    MissingColumn(String),
    #[error(
        "GeoParquet 'crs' is explicitly null (no CRS assigned); assuming OGC:CRS84 (lon/lat WGS84)"
    )]
    NullCrsDegrees,
    #[error("GeoParquet 'crs' is explicitly null and the declared bbox is outside lon/lat degree ranges; treating CRS as unknown")]
    NullCrsUnknown,
    #[error("Unexpected CRS format in GeoParquet metadata: {0}")]
    UnexpectedFormat(String),
    #[error("GeoParquet 'geo' metadata has no 'version' field (the spec requires one); reading it as GeoParquet 1.x")]
    MissingVersion,
    #[error("GeoParquet 'geo' metadata declares version {0:?}, which this version of tylertoo does not know (GeoParquet 1.x and 2.x are supported); reading it best-effort as 1.x — metadata fields may be misread")]
    UnknownVersion(String),
}

/// Why a bbox covering cannot replace geometry decoding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", content = "value", rename_all = "snake_case")]
pub enum CoveringWarning {
    NoCovering,
    UnsupportedPath,
    MissingColumn,
    DuplicateColumns,
    NotFloat64,
    NoFooterStatistics,
    MissingStatistics(usize),
    OutOfCrsRange(usize),
    UnknownCrs,
    InconsistentPartitions,
    NoUsableSample,
    ReadFailed(String),
}

impl fmt::Display for CoveringWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCovering => f.write_str("no bbox covering declared"),
            Self::UnsupportedPath => f.write_str(
                "unsupported covering path shape (expected four children of one struct)",
            ),
            Self::MissingColumn => f.write_str("declared covering column or child is missing"),
            Self::DuplicateColumns => f.write_str("covering paths reuse the same column"),
            Self::NotFloat64 => f.write_str(
                "covering children are not Float64; retaining exact geometry-derived bounds",
            ),
            Self::NoFooterStatistics => f.write_str("no Parquet footer statistics available"),
            Self::MissingStatistics(rg) => {
                write!(f, "row group {rg} lacks finite covering min/max statistics")
            }
            Self::OutOfCrsRange(rg) => write!(
                f,
                "row group {rg} covering statistics exceed the declared CRS coordinate range"
            ),
            Self::UnknownCrs => f.write_str("the declared CRS coordinate range is unknown"),
            Self::InconsistentPartitions => {
                f.write_str("partitions declare different covering columns")
            }
            Self::NoUsableSample => {
                f.write_str("covering yielded no usable bounds for any sampled row")
            }
            Self::ReadFailed(error) => write!(f, "could not read the covering: {error}"),
        }
    }
}

macro_rules! event_count {
    ($event:expr) => {{
        match $event {
            Self::OutOfRange { count, .. }
            | Self::Unprojectable { count, .. }
            | Self::Wrapped { count, .. }
            | Self::AntimeridianSuspect { count }
            | Self::EncodeDropped { count }
            | Self::EncodeQuantized { count }
            | Self::ClipGuardDropped { count }
            | Self::NonFiniteGeometry { count }
            | Self::PolygonRepaired { count, .. }
            | Self::InvalidAfterEncode { count }
            | Self::RowGroupAutoscaled { count, .. }
            | Self::InvalidAfterSimplify { count }
            | Self::FullResolutionFallback { count }
            | Self::ValidationSkipped { count }
            | Self::Merged { count }
            | Self::ValveDropped { count }
            | Self::PropertySkipped { count, .. }
            | Self::CrsMetadata { count, .. }
            | Self::CoveringRejected { count, .. }
            | Self::UnusableGeometry { count, .. }
            | Self::CrsAssumed { count, .. }
            | Self::EmptyLevels { count, .. }
            | Self::MemoryBudget { count, .. }
            | Self::Advisory { count, .. }
            | Self::SavedPlan { count, .. }
            | Self::PyramidSharedTiles { count }
            | Self::PyramidSkippedTiles { count }
            | Self::PyramidMissingBounds { count }
            | Self::PyramidMetadataConflict { count }
            | Self::PyramidMissingMetadata { count } => count,
        }
    }};
}

impl Diagnostic {
    /// Number of events represented by this aggregate.
    pub fn count(&self) -> usize {
        *event_count!(self)
    }
    fn count_mut(&mut self) -> &mut usize {
        event_count!(self)
    }
    fn accumulate(&mut self, event: &Self) {
        // Both callers select by same_kind first, which owns context
        // identity (including CRS). Only the counts and totals merge here.
        let count = self.count().saturating_add(event.count());
        match (&mut *self, event) {
            (Self::OutOfRange { context: a, .. }, Self::OutOfRange { context: b, .. })
            | (Self::Unprojectable { context: a, .. }, Self::Unprojectable { context: b, .. })
            | (Self::Wrapped { context: a, .. }, Self::Wrapped { context: b, .. }) => {
                *a = match (*a, *b) {
                    (Some(a), Some(b)) => Some(RangeContext {
                        total: a.total.saturating_add(b.total),
                        crs: a.crs,
                        max_abs: a.max_abs.max(b.max_abs),
                    }),
                    _ => None,
                };
            }
            (Self::UnusableGeometry { total: a, .. }, Self::UnusableGeometry { total: b, .. }) => {
                *a = a.saturating_add(*b)
            }
            _ => {}
        }
        *self.count_mut() = count;
    }

    fn same_counter_kind(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Advisory { message: a, .. }, Self::Advisory { message: b, .. }) => a == b,
            (Self::PolygonRepaired { kind: a, .. }, Self::PolygonRepaired { kind: b, .. }) => {
                a == b
            }
            (Self::CrsMetadata { reason: a, .. }, Self::CrsMetadata { reason: b, .. }) => a == b,
            (
                Self::CoveringRejected { reason: a, .. },
                Self::CoveringRejected { reason: b, .. },
            ) => a == b,
            (Self::PropertySkipped { name: a, .. }, Self::PropertySkipped { name: b, .. }) => {
                a == b
            }
            _ => std::mem::discriminant(self) == std::mem::discriminant(other),
        }
    }

    fn same_kind(&self, other: &Self) -> bool {
        // Counts and denominators aggregate; every other payload belongs to
        // its own context. Comparing normalized events also covers future
        // variants, rather than silently keeping their first payload.
        fn context_key(event: &Diagnostic) -> Diagnostic {
            let mut event = event.clone();
            *event.count_mut() = 0;
            match &mut event {
                Diagnostic::OutOfRange {
                    context: Some(c), ..
                }
                | Diagnostic::Wrapped {
                    context: Some(c), ..
                }
                | Diagnostic::Unprojectable {
                    context: Some(c), ..
                } => {
                    c.total = 0;
                    c.max_abs = 0.0;
                }
                Diagnostic::UnusableGeometry { total, .. } => *total = 0,
                _ => {}
            }
            event
        }
        context_key(self) == context_key(other)
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = self.count();
        match self {
            Self::OutOfRange {exemplars, context, ..} => {
                if let Some(context) = context {
                    f.write_str(&range_message(n, context.total, context.crs, context.max_abs, exemplars).unwrap_or_default())
                } else {
                    write!(f, "{n} feature(s) outside their declared CRS coordinate range; check the input CRS.{}", range_exemplars(exemplars))
                }
            }
            Self::Unprojectable {context,..} => f.write_str(&unprojectable_message(n, context.map_or(n, |c| c.total)).unwrap_or_default()),
            Self::Wrapped {context,exemplars,..} => {
                let context = context.unwrap_or(RangeContext {total: n, crs: Crs::Epsg4326, max_abs: 0.0});
                f.write_str(&wrapped_message(n, context.total, context.crs, exemplars).unwrap_or_default())
            }
            Self::AntimeridianSuspect {..} => write!(f, "{n} feature(s) span more than 180° longitude; possible antimeridian crossings"),
            Self::EncodeDropped {..} => write!(f, "{n} tile feature(s) had nothing to encode and were dropped at MVT encode"),
            Self::EncodeQuantized {..} => write!(f, "{n} tile feature(s) collapsed at the tile extent (clip slivers)"),
            Self::ClipGuardDropped {..} => write!(f, "{n} geometry/operation(s) with non-finite or absurdly large coordinates were left out by the clip guard (#707)"),
            Self::NonFiniteGeometry {..} => write!(f, "{n} geometry/row(s) with a NaN or infinite coordinate were read and left out (#707)"),
            Self::PolygonRepaired {kind,..} => write!(f, "{n} polygon(s) repaired in tile coordinates ({kind:?})"),
            Self::InvalidAfterEncode {..} => write!(f, "{n} polygon(s) still invalid in tile coordinates after repair; written as-is (#748)"),
            Self::RowGroupAutoscaled {cap,..} => write!(f, "--row-group-size raised to {cap} so the output stays under parquet's row-group ceiling; pass --row-group-size {cap} to make it explicit"),
            Self::InvalidAfterSimplify {..} => write!(f, "{n} invalid RDP candidate(s) required repair or epsilon retries"),
            Self::FullResolutionFallback {..} => write!(f, "{n} polygon(s) kept at full resolution after invalid RDP candidates"),
            Self::ValidationSkipped {..} => write!(f, "{n} oversized RDP candidate(s) skipped exact validity checking and were assumed valid (#242)"),
            Self::Merged {..} => write!(f, "{n} tile feature(s) merged by the size valve"),
            Self::ValveDropped {..} => write!(f, "{n} tile feature(s) dropped by the size valve"),
            Self::PropertySkipped {name,data_type,..} => write!(f, "property {name:?} ({data_type}) skipped: no MVT encoding"),
            Self::CrsMetadata {reason,..} => reason.fmt(f),
            Self::CoveringRejected {reason,..} => write!(f, "bbox covering fast path declined: {reason}; reading the geometry column"),
            Self::UnusableGeometry {total,..} => write!(f, "skipping {n} of {total} input rows with a null, empty, or non-finite geometry"),
            Self::CrsAssumed {input,..} => write!(f, "input {input}: no CRS declared in the GeoParquet metadata; assuming WGS84 lon/lat (EPSG:4326). If the coordinates are not lon/lat degrees the output will be misplaced — check the input with `gpio inspect {input}`"),
            Self::EmptyLevels {levels,gsd_min,gsd_max,input_features,first_written_gsd,first_written_zoom,..} => {
                let ids = levels.iter().map(usize::to_string).collect::<Vec<_>>().join(", ");
                let zoom = first_written_zoom.map_or_else(String::new, |z| format!(" (zoom {z})"));
                write!(f, "omitting {n} empty level(s) [{ids}] spanning GSD {gsd_max:.2}–{gsd_min:.2} m: none of the {input_features} input feature(s) are visible at those scales (visibility gates / density budget); the output pyramid starts at GSD {first_written_gsd:.2} m{zoom}. To populate coarse levels, lower --polygon-visibility/--line-visibility, or pass --collapse to keep sub-GSD polygons as representative points (see docs/OVERVIEW_TUNING.md)")
            }
            Self::SavedPlan {diagnostics,..} => {
                write!(f, "saved plan (whole input): {}", diagnostics.iter().map(ToString::to_string).collect::<Vec<_>>().join("; "))
            }
            Self::Advisory {message,..} => f.write_str(message),
            Self::MemoryBudget {machine,limit,..} => write!(f, "memory budget from cgroup limit: {limit} bytes (machine available: {machine:?} bytes)"),
            Self::PyramidSharedTiles {..} => write!(f, "{n} tile(s) shared across bands; counted once in the merged archive"),
            Self::PyramidSkippedTiles {..} => write!(f, "{n} tile(s) outside their band's declared zoom range skipped"),
            Self::PyramidMissingBounds {..} => write!(f, "{n} band archive(s) carried no usable bounds"),
            Self::PyramidMetadataConflict {..} => write!(f, "{n} publication metadata field(s) conflict across bands"),
            Self::PyramidMissingMetadata {..} => write!(f, "{n} band archive(s) carried no usable publication metadata"),
        }
    }
}

/// A disjoint source-feature loss, rather than a (feature, tile) counter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceLoss {
    /// Row index: selected input stream before assignment, or original overview file at export.
    /// Convert reports restore this to the original input row index.
    pub id: u64,
    /// Caller-provided MVT ID at export. It may repeat; `id` remains unique.
    #[serde(default)]
    pub feature_id: Option<u64>,
    /// Export zoom; absent for losses before level assignment.
    pub zoom: Option<u8>,
    /// Stage that removed the source feature.
    pub cause: SourceLossCause,
}

/// The stage responsible for an absent source feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceLossCause {
    NonFiniteGeometry,
    MissingGeometry,
    Filtered,
    OutsideBbox,
    EncodeDropped,
    Quantized,
    Merged,
    ValveDropped,
    ClipDropped,
}

/// Accounting must fail explicitly rather than truncate the loss partition.
#[derive(Debug, thiserror::Error)]
pub enum AccountingError {
    #[error("source accounting spill: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "source loss report needs {needed} bytes, exceeding its {available} byte memory budget"
    )]
    BudgetExceeded { needed: usize, available: usize },
}

/// A thread-safe sink borrowed by all stages of one operation.
#[derive(Debug)]
pub struct Diagnostics {
    emit_logs: bool,
    events: Mutex<Vec<Diagnostic>>,
    accounting: Mutex<accounting::Accounting>,
    input_rows: Mutex<Option<crate::input_set::StreamRowLocator>>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            emit_logs: true,
            events: Mutex::default(),
            accounting: Mutex::default(),
            input_rows: Mutex::default(),
        }
    }
}

impl Diagnostics {
    pub(crate) fn warn(&self, message: String) {
        self.once(Diagnostic::Advisory { count: 1, message });
    }

    /// Defer metadata warnings until the input is used by an operation.
    pub(crate) fn silent() -> Self {
        Self {
            emit_logs: false,
            ..Self::default()
        }
    }

    /// Track a loss by source row; a source drawn in any tile is never lost.
    ///
    /// # Errors
    /// Returns a spill I/O error if the row cannot be recorded.
    pub fn source_loss(
        &self,
        row: u64,
        zoom: Option<u8>,
        cause: SourceLossCause,
    ) -> Result<(), AccountingError> {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(row, None, zoom, Some(cause))
    }
    /// Track an exported row separately from its possibly duplicated MVT ID.
    pub(crate) fn source_member_loss(
        &self,
        row: u64,
        id: u64,
        zoom: u8,
        cause: SourceLossCause,
    ) -> Result<(), AccountingError> {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(row, Some(id), Some(zoom), Some(cause))
    }
    /// A source survives if any tile member survives the final valve.
    ///
    /// # Errors
    /// Returns a spill I/O error if the row cannot be recorded.
    pub fn source_emitted(&self, row: u64, zoom: u8) -> Result<(), AccountingError> {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record(row, None, Some(zoom), None)
    }
    /// Disjoint losses, with convert rows restored to original input ordinals.
    ///
    /// # Errors
    /// Returns a spill I/O error, an invalid row address, or a typed budget
    /// error when materializing the complete loss report would exceed its limit.
    pub fn source_losses(&self) -> Result<Vec<SourceLoss>, AccountingError> {
        let mut losses = self.source_losses_for_plan()?;
        if let Some(locator) = &*self.input_rows.lock().unwrap_or_else(|e| e.into_inner()) {
            for loss in &mut losses {
                if loss.zoom.is_none() {
                    loss.id = locator.global_row(loss.id as usize).ok_or_else(|| {
                        std::io::Error::other("source loss is outside the selected input rows")
                    })?;
                }
            }
        }
        Ok(losses)
    }
    /// Plans address their winner tables in the selected stream, before the
    /// report restores original file positions. Shard replay rebases once.
    pub(crate) fn source_losses_for_plan(&self) -> Result<Vec<SourceLoss>, AccountingError> {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .losses()
    }
    pub(crate) fn set_input_rows(&self, locator: crate::input_set::StreamRowLocator) {
        *self.input_rows.lock().unwrap_or_else(|e| e.into_inner()) = Some(locator);
    }
    pub(crate) fn source_row_ranges(&self) -> Vec<(u64, u64)> {
        self.input_rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or_else(Vec::new, |r| r.ranges())
    }
    pub(crate) fn input_row_count(&self) -> u64 {
        self.input_rows
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, |r| r.len())
    }
    pub(crate) fn set_accounting_spill_dir(&self, path: Option<&std::path::Path>) {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_spill_dir(path);
    }
    pub(crate) fn set_accounting_budget(&self, bytes: usize) {
        self.accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_budget(bytes);
    }
    #[cfg(test)]
    fn resident_accounting_bytes(&self) -> usize {
        self.accounting.lock().unwrap().resident_bytes()
    }

    /// Aggregate completed child runs without logging their warnings again.
    pub fn extend(&self, events: impl IntoIterator<Item = Diagnostic>) {
        let mut existing = self.events.lock().unwrap_or_else(|e| e.into_inner());
        for event in events {
            if let Some(old) = existing.iter_mut().find(|old| old.same_kind(&event)) {
                old.accumulate(&event);
            } else {
                existing.push(event);
            }
        }
    }

    /// Aggregate an event. The first occurrence logs once in this run.
    pub fn record(&self, event: Diagnostic) {
        self.insert(event, false);
    }
    /// Record a warning once in this run, even for multipart input.
    pub fn once(&self, event: Diagnostic) {
        self.insert(event, true);
    }
    fn insert(&self, event: Diagnostic, once: bool) {
        if event.count() == 0 {
            return;
        }
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(existing) = events.iter_mut().find(|e| e.same_kind(&event)) {
            if !once {
                existing.accumulate(&event);
            }
        } else {
            if self.emit_logs {
                match &event {
                    Diagnostic::Wrapped { .. }
                    | Diagnostic::EncodeQuantized { .. }
                    | Diagnostic::PolygonRepaired { .. }
                    | Diagnostic::InvalidAfterSimplify { .. }
                    | Diagnostic::FullResolutionFallback { .. }
                    | Diagnostic::MemoryBudget { .. }
                    | Diagnostic::ValidationSkipped { .. } => log::info!("{event}"),
                    Diagnostic::CoveringRejected {
                        reason:
                            CoveringWarning::NoCovering
                            | CoveringWarning::UnsupportedPath
                            | CoveringWarning::NotFloat64
                            | CoveringWarning::NoFooterStatistics,
                        ..
                    } => log::debug!("{event}"),
                    _ => log::warn!("{event}"),
                }
            }
            events.push(event);
        }
    }
    /// Deterministic aggregate order, independent of worker scheduling.
    pub fn snapshot(&self) -> Vec<Diagnostic> {
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        events.sort_by_cached_key(|event| serde_json::to_string(event).unwrap_or_default());
        events
    }
    /// Plans store source facts; memory advice belongs to the current process.
    pub(crate) fn snapshot_for_plan(&self) -> Vec<Diagnostic> {
        self.snapshot()
            .into_iter()
            .filter(|event| {
                !matches!(
                    event,
                    Diagnostic::MemoryBudget { .. } | Diagnostic::Advisory { .. }
                )
            })
            .collect()
    }

    /// Sum one kind of event without maintaining a parallel counter.
    pub fn count(&self, kind: &Diagnostic) -> usize {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|e| e.same_counter_kind(kind))
            .map(Diagnostic::count)
            .sum()
    }
}

fn wrapped_message(
    count: usize,
    total: usize,
    crs: Crs,
    exemplars: &[OutOfRangeExemplar],
) -> Option<String> {
    if count == 0 || total == 0 {
        return None;
    }
    let edge = match crs {
        Crs::Epsg4326 => "±180° longitude",
        Crs::Epsg3857 => "the ±20037508.34 m world edge in x",
    };
    let pct = count as f64 / total as f64 * 100.0;
    let exemplar = exemplar_text(exemplars, ExemplarEdge::Wrap);
    let stop = if exemplar.is_empty() { "" } else { "." };
    Some(format!(
        "{count} of {total} feature(s) ({pct:.1}%) reach past {edge} and are drawn \
         wrapped across the antimeridian.{exemplar}{stop} That is expected for 0..360 data \
         and dateline cells; otherwise check that the coordinates really are the \
         declared CRS."
    ))
}

fn range_exemplars(exemplars: &[OutOfRangeExemplar]) -> String {
    exemplar_text(exemplars, ExemplarEdge::OutOfRange)
}

fn exemplar_text(exemplars: &[OutOfRangeExemplar], edge: ExemplarEdge) -> String {
    if exemplars.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = exemplars
        .iter()
        .map(|e| {
            let value = exemplar_value_text(e.value, edge.limit(e.axis));
            match e.part {
                None => format!("{} {value} (row {})", e.axis, e.row),
                Some(p) => format!("{} {value} (part {p}, row {})", e.axis, e.row),
            }
        })
        .collect();
    format!(" e.g. {}", parts.join(", "))
}

fn exemplar_value_text(value: f64, limit: f64) -> String {
    let short = format!("{value:.3}");
    match short.parse::<f64>() {
        Ok(rounded) if rounded.abs() > limit => short,
        _ => format!("{value}"),
    }
}

fn range_message(
    count: usize,
    total: usize,
    crs: Crs,
    max_abs: f64,
    exemplars: &[OutOfRangeExemplar],
) -> Option<String> {
    if count == 0 || total == 0 {
        return None;
    }
    let range = crs_coordinate_range(crs);
    let (label, text) = (range.label, range.text);
    let pct = count as f64 / total as f64 * 100.0;
    let advice = reprojection_advice(crs, max_abs);
    let exemplar = range_exemplars(exemplars);
    Some(format!(
        "{count} of {total} feature(s) ({pct:.1}%) reach beyond the {label} coordinate \
         range ({text}) and were dropped or clipped.{exemplar}{advice}"
    ))
}

fn unprojectable_message(count: usize, total: usize) -> Option<String> {
    if count == 0 || total == 0 {
        return None;
    }
    let pct = count as f64 / total as f64 * 100.0;
    Some(format!(
        "{count} of {total} feature(s) ({pct:.1}%) have valid lon/lat but lie outside the \
         Web Mercator tiling domain (|lat| > 85.05°); these features cannot be tiled. \
         Web Mercator does not reach the poles — clip the input to |lat| ≤ 85.05° or use \
         a polar tiling scheme instead."
    ))
}

#[derive(Debug, Clone, Copy)]
enum ExemplarEdge {
    /// [`bbox_out_of_crs_range`]'s edges: x and longitude carry the export's
    /// one-world wrap (#342).
    OutOfRange,
    /// [`bbox_wraps`]'s edge: the world's own ±180° (#644).
    Wrap,
}

impl ExemplarEdge {
    /// The `|value|` an exemplar on `axis` lies beyond.
    fn limit(self, axis: &str) -> f64 {
        let wrap = match self {
            Self::OutOfRange => 3.0,
            Self::Wrap => 1.0,
        };
        match axis {
            "lon" => wrap * 180.0,
            "lat" => 90.0,
            "x" => wrap * WEBMERC_HALF_M,
            _ => WEBMERC_HALF_M,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_accounting_residency_is_bounded() {
        let sink = Diagnostics::default();
        for id in 0..100_000 {
            sink.source_loss(id, Some(4), SourceLossCause::ClipDropped)
                .unwrap();
            sink.source_emitted(id, 4).unwrap();
        }
        assert!(sink.resident_accounting_bytes() <= 1 << 20);
        assert!(sink.source_losses().unwrap().is_empty());
    }

    #[test]
    fn oversized_loss_reports_return_a_typed_budget_error() {
        let sink = Diagnostics::default();
        sink.set_accounting_budget(std::mem::size_of::<SourceLoss>());
        sink.source_loss(0, None, SourceLossCause::Filtered)
            .unwrap();
        sink.source_loss(1, None, SourceLossCause::Filtered)
            .unwrap();
        assert!(matches!(
            sink.source_losses(),
            Err(AccountingError::BudgetExceeded { .. })
        ));
    }

    #[test]
    fn accounting_spills_to_the_requested_volume_and_reports_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        let sink = Diagnostics::default();
        sink.set_accounting_spill_dir(Some(dir.path()));
        std::fs::remove_dir(dir.path()).unwrap();
        assert!(matches!(
            sink.source_loss(10_000, None, SourceLossCause::Filtered),
            Err(AccountingError::Io(_))
        ));
    }

    #[test]
    fn child_run_aggregation_combines_context_totals() {
        let sink = Diagnostics::default();
        let event = Diagnostic::Unprojectable {
            count: 1,
            context: Some(RangeContext {
                total: 10,
                crs: Crs::Epsg4326,
                max_abs: 0.0,
            }),
        };
        sink.extend([event.clone(), event]);
        assert!(sink.snapshot()[0]
            .to_string()
            .contains("2 of 20 feature(s) (10.0%)"));
    }

    #[test]
    fn persisted_diagnostics_exclude_process_memory_advice() {
        let sink = Diagnostics::default();
        sink.record(Diagnostic::MemoryBudget {
            count: 1,
            machine: Some(99),
            limit: 10,
        });
        sink.record(Diagnostic::NonFiniteGeometry { count: 2 });
        assert_eq!(
            sink.snapshot_for_plan(),
            vec![Diagnostic::NonFiniteGeometry { count: 2 }]
        );
        assert_eq!(
            sink.snapshot().len(),
            2,
            "reports retain the run's memory advice"
        );
    }
}
