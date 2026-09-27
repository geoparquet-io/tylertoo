//! `--feature-id <COLUMN>` (#443): a property column carried through as the
//! MVT feature `id` on every tile and zoom a feature appears in.
//!
//! Three steps, all before any tile is clipped:
//!
//! 1. [`resolve_feature_id`] finds the column (by its *published* name),
//!    gates its type, rejects the pairings that cannot work, and withholds it
//!    from the tile properties -- the id is moved, never copied.
//! 2. [`validate_feature_id_column`] checks **every row of the overview
//!    file**, all levels, in one column-projected read (row groups whose
//!    Parquet statistics already prove validity are skipped), so a bad value
//!    fails the export in seconds, naming its true overview-file row and
//!    level, instead of after the coarser levels have been exported.
//! 3. Pass 2 reads the already-validated column per batch through
//!    [`feature_id_values`].
//!
//! DIVERGENCE FROM TIPPECANOE: `--use-attribute-for-id` accepts any numeric
//! attribute, including integral doubles and numeric strings (fed through
//! `strtoull`), and only *warns* on a non-numeric or fractional one; a
//! leading `-` is reinterpreted as the two's-complement unsigned value
//! whenever printing it back reproduces the input (`serial.cpp`'s
//! `attribute_for_id` block), so `"-5"` silently becomes id
//! `18446744073709551611`. We accept only integer columns (`Int8`..`Int64`,
//! `UInt8`..`UInt64`) and unscaled decimals (`DECIMAL(p,0)`, a common ID
//! carrier) and make null, negative and out-of-`u64`-range values hard
//! errors, as #443 asks. String and float columns are rejected rather than
//! parsed: a string id column (Overture's GERS ids, for example) has no
//! lossless `u64` form, and a float column's integrality is a per-value
//! accident. Cast such a column to an integer first (`gpio`, or DuckDB's
//! `CAST(col AS UBIGINT)` / a hash) -- see `context/ARCHITECTURE.md`.

use std::fmt;

use arrow_array::cast::AsArray;
use arrow_array::types::{
    Decimal128Type, Decimal256Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type,
    UInt32Type, UInt64Type, UInt8Type,
};
use arrow_array::Array;
use arrow_schema::{DataType, Schema};
use parquet::file::statistics::Statistics;

#[cfg(test)]
use crate::mvt::PropertyValue;

use super::super::level::OverviewsMeta;
use super::super::reader::OverviewReader;
use super::super::writer::LEVEL_COLUMN;
use super::{
    is_supported_scalar, ExportError, ExportOptions, FeatureOrder, PublishedNames,
    EXPORT_BATCH_SIZE,
};

/// Why a `--feature-id` column cannot be used at all (#443); carried by
/// [`ExportError::FeatureIdColumn`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum FeatureIdColumnProblem {
    /// The overview file has no such column. `available` lists every
    /// property column (non-geometry, non-`level`, MVT-encodable type) by its
    /// published name -- including columns the property selection or the
    /// #379 heuristic suppressed, which `--feature-id` can still name.
    NotFound {
        /// Comma-separated, quoted published names.
        available: String,
    },
    /// The column's Arrow type cannot represent a `uint64` MVT feature id.
    NotInteger {
        /// The column's Arrow data type, as `Debug`-formatted text.
        data_type: String,
    },
    /// `--feature-order` names the same column: `--feature-id` removes it
    /// from the tile properties that sort reads.
    ConflictsWithFeatureOrder,
    /// The column was aggregated across clustered points at convert time
    /// (`--accumulate-attribute`), so a cluster's value is a sum/min/max/mean
    /// of several ids -- an id of no feature.
    Accumulated {
        /// The aggregation operator recorded in the file's clustering
        /// provenance (`sum`, `min`, `max`, `mean`).
        op: String,
    },
}

impl fmt::Display for FeatureIdColumnProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound { available } => write!(
                f,
                "is not a column of the overview file (available: {available})"
            ),
            Self::NotInteger { data_type } => write!(
                f,
                "has type {data_type}, but MVT feature ids are unsigned 64-bit integers: use an \
                 integer column (Int8..Int64, UInt8..UInt64) or DECIMAL(p,0); cast a string or \
                 float id to an integer first (e.g. with gpio or DuckDB)"
            ),
            Self::ConflictsWithFeatureOrder => f.write_str(
                "is also the --feature-order column: --feature-id removes it from the tile \
                 properties --feature-order sorts by, so the two cannot share a column",
            ),
            Self::Accumulated { op } => write!(
                f,
                "was aggregated across clustered points (--accumulate-attribute {op}), so a \
                 cluster's value identifies no single feature; accumulate a different column"
            ),
        }
    }
}

/// Why one row's `--feature-id` value cannot be an MVT feature id (#443);
/// carried by [`ExportError::InvalidFeatureId`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum InvalidFeatureIdReason {
    /// The value is null: every exported feature needs an id.
    Null,
    /// The value is negative; tippecanoe would silently reinterpret it as a
    /// huge unsigned id instead.
    Negative {
        /// The value, as text.
        value: String,
    },
    /// A `DECIMAL(p,0)` value above `u64::MAX`.
    TooLarge {
        /// The value, as text.
        value: String,
    },
}

impl fmt::Display for InvalidFeatureIdReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("is null"),
            Self::Negative { value } => write!(f, "is negative ({value})"),
            Self::TooLarge { value } => write!(f, "exceeds the u64 maximum ({value})"),
        }
    }
}

/// The resolved `--feature-id` column: its index in the overview file's
/// schema (which every pass-2 batch shares) and its published name, for
/// error messages. Resolved once per export.
#[derive(Debug, Clone)]
pub(super) struct ResolvedFeatureId {
    pub(super) idx: usize,
    pub(super) name: String,
}

fn column_error(name: &str, reason: FeatureIdColumnProblem) -> ExportError {
    ExportError::FeatureIdColumn {
        column: name.to_string(),
        reason,
    }
}

/// Resolve `options.feature_id` against the overview file, or `Ok(None)`
/// when it is unset, and withhold the column from the tile properties and
/// `vector_layers` metadata.
///
/// Matched by *published* name -- the naming `--feature-order` /
/// `--include-property` / `--exclude-property` use -- and independent of
/// any suppression already recorded on `published`: an explicit
/// `--feature-id` always wins over the #379 auto-suppression heuristic and
/// over an `--include-property` / `--exclude-property` naming the same
/// column, mirroring tippecanoe, whose `attribute_for_id` block extracts the
/// id before its own `-x`/`-y`/`-X` filter is consulted (`serial.cpp`). So
/// naming the id column in either selection flag is a harmless no-op.
pub(super) fn resolve_feature_id(
    schema: &Schema,
    geom_idx: usize,
    meta: &OverviewsMeta,
    options: &ExportOptions,
    published: &mut PublishedNames,
) -> Result<Option<ResolvedFeatureId>, ExportError> {
    let Some(name) = &options.feature_id else {
        return Ok(None);
    };
    if matches!(&options.feature_order, FeatureOrder::Column { name: order, .. } if order == name) {
        return Err(column_error(
            name,
            FeatureIdColumnProblem::ConflictsWithFeatureOrder,
        ));
    }
    let candidates = || {
        schema
            .fields()
            .iter()
            .enumerate()
            .filter(|&(i, f)| i != geom_idx && !f.name().eq_ignore_ascii_case(LEVEL_COLUMN))
    };
    let Some((idx, field)) = candidates().find(|(_, f)| published.publish(f.name()) == name) else {
        let available = candidates()
            .filter(|(_, f)| is_supported_scalar(f.data_type()))
            .map(|(_, f)| format!("{:?}", published.publish(f.name())))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(column_error(
            name,
            FeatureIdColumnProblem::NotFound { available },
        ));
    };
    if !is_feature_id_type(field.data_type()) {
        return Err(column_error(
            name,
            FeatureIdColumnProblem::NotInteger {
                data_type: format!("{:?}", field.data_type()),
            },
        ));
    }
    let accumulated = meta
        .generalization
        .as_ref()
        .and_then(|g| g.clustering.as_ref())
        .and_then(|c| {
            c.accumulated
                .iter()
                .find(|a| a.column == *field.name() || a.column == *name)
        });
    if let Some(a) = accumulated {
        return Err(column_error(
            name,
            FeatureIdColumnProblem::Accumulated { op: a.op.clone() },
        ));
    }
    published.suppress(field.name());
    Ok(Some(ResolvedFeatureId {
        idx,
        name: name.clone(),
    }))
}

/// The Arrow types a `--feature-id` column may have: every integer type (a
/// signed column's negative values are rejected per row, since GeoParquet
/// integer ids are usually signed) and unscaled decimals.
fn is_feature_id_type(dt: &DataType) -> bool {
    matches!(
        dt,
        DataType::Int8
            | DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::UInt8
            | DataType::UInt16
            | DataType::UInt32
            | DataType::UInt64
            | DataType::Decimal128(_, 0)
            | DataType::Decimal256(_, 0)
    )
}

/// One `--feature-id` column's values for a batch: the `u64` id per row, or
/// why that row has none. A column of a type [`resolve_feature_id`] rejects
/// yields `None` (unreachable once resolution has passed).
pub(super) fn feature_id_values(
    col: &dyn Array,
) -> Option<Vec<Result<u64, InvalidFeatureIdReason>>> {
    let n = col.len();
    let negative = |value: String| InvalidFeatureIdReason::Negative { value };
    macro_rules! signed {
        ($ty:ty) => {{
            let a = col.as_primitive::<$ty>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        return Err(InvalidFeatureIdReason::Null);
                    }
                    let v = i64::from(a.value(i));
                    u64::try_from(v).map_err(|_| negative(v.to_string()))
                })
                .collect()
        }};
    }
    macro_rules! unsigned {
        ($ty:ty) => {{
            let a = col.as_primitive::<$ty>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        Err(InvalidFeatureIdReason::Null)
                    } else {
                        Ok(u64::from(a.value(i)))
                    }
                })
                .collect()
        }};
    }
    let values = match col.data_type() {
        DataType::Int8 => signed!(Int8Type),
        DataType::Int16 => signed!(Int16Type),
        DataType::Int32 => signed!(Int32Type),
        DataType::Int64 => signed!(Int64Type),
        DataType::UInt8 => unsigned!(UInt8Type),
        DataType::UInt16 => unsigned!(UInt16Type),
        DataType::UInt32 => unsigned!(UInt32Type),
        DataType::UInt64 => unsigned!(UInt64Type),
        DataType::Decimal128(_, 0) => {
            let a = col.as_primitive::<Decimal128Type>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        return Err(InvalidFeatureIdReason::Null);
                    }
                    let v = a.value(i);
                    u64::try_from(v).map_err(|_| decimal_out_of_range(v < 0, v.to_string()))
                })
                .collect()
        }
        DataType::Decimal256(_, 0) => {
            let a = col.as_primitive::<Decimal256Type>();
            (0..n)
                .map(|i| {
                    if a.is_null(i) {
                        return Err(InvalidFeatureIdReason::Null);
                    }
                    let v = a.value(i);
                    v.to_i128()
                        .and_then(|w| u64::try_from(w).ok())
                        .ok_or_else(|| decimal_out_of_range(v.is_negative(), v.to_string()))
                })
                .collect()
        }
        _ => return None,
    };
    Some(values)
}

fn decimal_out_of_range(negative: bool, value: String) -> InvalidFeatureIdReason {
    if negative {
        InvalidFeatureIdReason::Negative { value }
    } else {
        InvalidFeatureIdReason::TooLarge { value }
    }
}

/// One row group the up-front check has to read, and where its rows sit.
struct Span {
    rg: usize,
    /// Overview-file row index of the row group's first row.
    first_row: u64,
    rows: u64,
    level: usize,
}

/// Check every row of the overview file -- every level -- holds a valid
/// `--feature-id` value, before any tile is clipped.
///
/// A column-projected read of only the row groups whose Parquet statistics
/// do not already prove validity (a zero null count, plus a non-negative
/// minimum for a signed column). The first bad value fails with its
/// overview-file row index (0-based, the file's own row order -- not the
/// source file's, which the convert reordered) and the level that owns it.
pub(super) fn validate_feature_id_column(
    reader: &OverviewReader,
    fid: &ResolvedFeatureId,
    level_zooms: &[u8],
) -> Result<(), ExportError> {
    let md = reader.parquet_metadata();
    let descr = md.file_metadata().schema_descr();
    let leaf = (0..descr.num_columns()).find(|&i| descr.get_column_root_idx(i) == fid.idx);
    let unsigned = matches!(
        reader.schema().field(fid.idx).data_type(),
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64
    );
    let levels = &reader.meta().levels;

    let mut spans = Vec::new();
    let mut first_row = 0u64;
    let mut level = 0usize;
    for rg in 0..md.num_row_groups() {
        while level + 1 < levels.len() && rg as i64 > levels[level].row_group_end {
            level += 1;
        }
        let rgm = md.row_group(rg);
        let rows = rgm.num_rows().max(0) as u64;
        let proven = leaf.is_some_and(|l| stats_prove_valid(rgm.column(l).statistics(), unsigned));
        if rows > 0 && !proven {
            spans.push(Span {
                rg,
                first_row,
                rows,
                level,
            });
        }
        first_row += rows;
    }
    if spans.is_empty() {
        return Ok(());
    }

    let batches = reader.read_row_groups_projected(
        spans.iter().map(|s| s.rg).collect(),
        EXPORT_BATCH_SIZE,
        &[fid.idx],
    )?;
    let (mut span, mut in_span) = (0usize, 0u64);
    for batch in batches {
        let batch = batch?;
        let values = feature_id_values(batch.column(0)).ok_or_else(|| {
            column_error(
                &fid.name,
                FeatureIdColumnProblem::NotInteger {
                    data_type: format!("{:?}", batch.column(0).data_type()),
                },
            )
        })?;
        for value in values {
            // Batches may straddle row groups; step to the span holding this row.
            while in_span == spans[span].rows {
                span += 1;
                in_span = 0;
            }
            if let Err(reason) = value {
                let s = &spans[span];
                return Err(ExportError::InvalidFeatureId {
                    column: fid.name.clone(),
                    level: s.level,
                    zoom: level_zooms[s.level],
                    row: s.first_row + in_span,
                    reason,
                });
            }
            in_span += 1;
        }
    }
    Ok(())
}

/// Whether a row group's column statistics alone prove every value is a
/// valid id: no nulls, and (for a signed column) a non-negative minimum.
fn stats_prove_valid(stats: Option<&Statistics>, unsigned: bool) -> bool {
    let Some(stats) = stats else {
        return false;
    };
    if stats.null_count_opt() != Some(0) {
        return false;
    }
    if unsigned {
        return true;
    }
    match stats {
        Statistics::Int32(s) => s.min_opt().is_some_and(|m| *m >= 0),
        Statistics::Int64(s) => s.min_opt().is_some_and(|m| *m >= 0),
        _ => false,
    }
}

/// Test-support: the [`super::encode_level_tiles`] reference path's id, read
/// from a feature's `(name, value)` list (it has no Arrow column). `row` is
/// the feature's index in that slice.
#[cfg(test)]
pub(super) fn feature_id_from_property(
    value: Option<&PropertyValue>,
    column: &str,
    zoom: u8,
    row: u64,
) -> Result<u64, ExportError> {
    let invalid = |reason| ExportError::InvalidFeatureId {
        column: column.to_string(),
        level: 0,
        zoom,
        row,
        reason,
    };
    match value {
        None => Err(invalid(InvalidFeatureIdReason::Null)),
        Some(PropertyValue::UInt(u)) => Ok(*u),
        Some(PropertyValue::Int(i)) => u64::try_from(*i).map_err(|_| {
            invalid(InvalidFeatureIdReason::Negative {
                value: i.to_string(),
            })
        }),
        Some(other) => Err(column_error(
            column,
            FeatureIdColumnProblem::NotInteger {
                data_type: format!("{other:?}"),
            },
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use arrow_array::{Decimal128Array, Int32Array, UInt64Array};
    use arrow_schema::Field;

    #[test]
    fn values_accept_nonnegative_integers_and_flag_null_and_negative() {
        let col = Int32Array::from(vec![Some(42), Some(0), None, Some(-5)]);
        assert_eq!(
            feature_id_values(&col).unwrap(),
            vec![
                Ok(42),
                Ok(0),
                Err(InvalidFeatureIdReason::Null),
                Err(InvalidFeatureIdReason::Negative {
                    value: "-5".to_string()
                }),
            ]
        );
        let col = UInt64Array::from(vec![Some(u64::MAX), None]);
        assert_eq!(
            feature_id_values(&col).unwrap(),
            vec![Ok(u64::MAX), Err(InvalidFeatureIdReason::Null)]
        );
    }

    /// `DECIMAL(38,0)` is a common ID carrier: its values convert exactly
    /// when they fit a `u64`, and say which way they do not.
    #[test]
    fn values_accept_unscaled_decimals_within_u64() {
        let big = i128::from(u64::MAX);
        let col = Decimal128Array::from(vec![Some(7), Some(big), Some(big + 1), Some(-1)])
            .with_precision_and_scale(38, 0)
            .unwrap();
        assert_eq!(
            feature_id_values(&col).unwrap(),
            vec![
                Ok(7),
                Ok(u64::MAX),
                Err(InvalidFeatureIdReason::TooLarge {
                    value: (big + 1).to_string()
                }),
                Err(InvalidFeatureIdReason::Negative {
                    value: "-1".to_string()
                }),
            ]
        );
    }

    #[test]
    fn id_types_are_integers_and_unscaled_decimals_only() {
        for ok in [
            DataType::Int8,
            DataType::Int64,
            DataType::UInt64,
            DataType::Decimal128(38, 0),
            DataType::Decimal256(76, 0),
        ] {
            assert!(is_feature_id_type(&ok), "{ok:?}");
        }
        for bad in [
            DataType::Utf8,
            DataType::Float64,
            DataType::Boolean,
            DataType::Decimal128(10, 2),
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Int64)),
        ] {
            assert!(!is_feature_id_type(&bad), "{bad:?}");
        }
    }

    fn schema() -> Schema {
        Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("name", DataType::Utf8, false),
            Field::new("level", DataType::Int32, false),
            Field::new("geometry", DataType::Binary, true),
        ])
    }

    fn opts(name: &str) -> ExportOptions {
        ExportOptions {
            feature_id: Some(name.to_string()),
            ..Default::default()
        }
    }

    fn meta() -> OverviewsMeta {
        serde_json::from_value(serde_json::json!({
            "version": "0.1.0",
            "levels": [{"gsd": 1.0, "row_group_end": 0}],
        }))
        .unwrap()
    }

    /// Resolution matches by published name, ignores suppression already
    /// recorded (an `--exclude-property` of the id column is a no-op), and
    /// suppresses the column itself.
    #[test]
    fn resolves_independent_of_suppression_and_suppresses_the_column() {
        let mut published = PublishedNames::identity();
        published.suppress("id");
        let fid = resolve_feature_id(&schema(), 3, &meta(), &opts("id"), &mut published)
            .unwrap()
            .unwrap();
        assert_eq!((fid.idx, fid.name.as_str()), (0, "id"));
        assert!(published.is_suppressed("id"));

        let mut published = PublishedNames::identity();
        resolve_feature_id(&schema(), 3, &meta(), &opts("id"), &mut published).unwrap();
        assert!(published.is_suppressed("id"), "the id is moved, not copied");
    }

    /// An unknown column lists every candidate, suppressed ones included,
    /// but never the geometry or `level`.
    #[test]
    fn unknown_column_lists_every_candidate_including_suppressed_ones() {
        let mut published = PublishedNames::identity();
        published.suppress("name");
        let err =
            resolve_feature_id(&schema(), 3, &meta(), &opts("nope"), &mut published).unwrap_err();
        match err {
            ExportError::FeatureIdColumn {
                column,
                reason: FeatureIdColumnProblem::NotFound { available },
            } => {
                assert_eq!(column, "nope");
                assert_eq!(available, "\"id\", \"name\"");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn accumulated_column_is_rejected() {
        let meta: OverviewsMeta = serde_json::from_value(serde_json::json!({
            "version": "0.1.0",
            "levels": [{"gsd": 1.0, "row_group_end": 0}],
            "generalization": {
                "engine": "tylertoo",
                "levels": [],
                "clustering": {
                    "enabled": true,
                    "point_count_column": "point_count",
                    "accumulated": [{"column": "id", "op": "sum"}],
                },
            },
        }))
        .unwrap();
        let err = resolve_feature_id(
            &schema(),
            3,
            &meta,
            &opts("id"),
            &mut PublishedNames::identity(),
        )
        .unwrap_err();
        assert!(
            matches!(
                &err,
                ExportError::FeatureIdColumn {
                    reason: FeatureIdColumnProblem::Accumulated { op },
                    ..
                } if op == "sum"
            ),
            "{err:?}"
        );
        assert!(err.to_string().contains("--accumulate-attribute sum"));
    }

    #[test]
    fn unset_is_a_no_op() {
        let mut published = PublishedNames::identity();
        let r = resolve_feature_id(
            &schema(),
            3,
            &meta(),
            &ExportOptions::default(),
            &mut published,
        )
        .unwrap();
        assert!(r.is_none());
        assert!(!published.is_suppressed("id"));
    }
}
