//! Arrow stream input (#824): stage a one-shot Arrow stream to a GeoParquet
//! file so the two-pass pipeline can read it twice.
//!
//! The staged file goes in `spill_dir` (else the system temp dir), the
//! directory a remote input's disk spill uses, and is removed when the
//! [`StagedStream`] drops, whether the run succeeded or failed.

use std::path::Path;
use std::sync::Arc;

use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::{DataType, Field, Schema};
use geoarrow::datatypes::Crs;
use geoarrow::error::GeoArrowResult;
use geoarrow_schema::crs::CrsTransform;
use geoparquet::writer::{
    GeoParquetRecordBatchEncoder, GeoParquetWriterEncoding, GeoParquetWriterOptionsBuilder,
};
use serde_json::Value;

use crate::overview::convert::ConvertError;

/// Rows per staged row group: DuckDB's default, so a large stream never
/// buffers the Parquet default's 1M-row group in memory (#829).
pub const STAGE_ROW_GROUP_ROWS: usize = 122_880;

const EXTENSION_NAME: &str = "ARROW:extension:name";
const EXTENSION_METADATA: &str = "ARROW:extension:metadata";

/// A staged stream: a GeoParquet file in a private (0700) directory,
/// deleted with it on drop.
#[derive(Debug)]
pub struct StagedStream(crate::atomic_output::Reserved);

impl StagedStream {
    /// The staged GeoParquet file.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// Writes the stream's CRS into the `geo` metadata as given: PROJJSON as an
/// object, any other identifier as its string, which the reader classifies
/// ([`crate::quality::CrsKind`]). No CRS becomes an explicit `null`, which
/// the reader treats like an unlabeled file, not as a declared OGC:CRS84.
/// geoparquet's default transform would drop a non-PROJJSON CRS, silently
/// turning it into CRS84.
#[derive(Debug)]
struct KeepCrs;

impl CrsTransform for KeepCrs {
    fn _convert_to_projjson(&self, crs: &Crs) -> GeoArrowResult<Option<Value>> {
        Ok(Some(crs.crs_value().cloned().unwrap_or(Value::Null)))
    }

    fn _convert_to_wkt(&self, _crs: &Crs) -> GeoArrowResult<Option<String>> {
        Ok(None)
    }
}

fn stream_error(message: impl Into<String>) -> ConvertError {
    ConvertError::ArrowStream(message.into())
}

/// View types as their offset forms: Polars and DuckDB stream strings as
/// `Utf8View`, which the export cannot encode as MVT values.
/// shortcut: top-level columns only; nested views pass through until a
/// stream needs them.
fn unview(data_type: &DataType) -> DataType {
    match data_type {
        DataType::Utf8View => DataType::LargeUtf8,
        DataType::BinaryView => DataType::LargeBinary,
        other => other.clone(),
    }
}

/// Tag the geometry field as GeoArrow, with `crs` when the stream has none.
fn geometry_field(field: &Field, crs: Option<&str>) -> Result<Field, ConvertError> {
    let mut metadata = field.metadata().clone();
    if field.extension_type_name().is_none() {
        if !matches!(field.data_type(), DataType::Binary | DataType::LargeBinary) {
            return Err(stream_error(format!(
                "geometry column {:?} is {}, not WKB binary or a GeoArrow type",
                field.name(),
                field.data_type()
            )));
        }
        metadata.insert(EXTENSION_NAME.into(), "geoarrow.wkb".into());
    }
    let mut extension: serde_json::Map<String, Value> = match metadata.get(EXTENSION_METADATA) {
        Some(json) if !json.is_empty() => serde_json::from_str(json).map_err(|e| {
            stream_error(format!(
                "geometry column {:?} has invalid GeoArrow metadata: {e}",
                field.name()
            ))
        })?,
        _ => serde_json::Map::new(),
    };
    let declared = extension.get("crs").is_some_and(|c| !c.is_null());
    match (declared, crs) {
        (true, Some(_)) => {
            return Err(stream_error(format!(
                "geometry column {:?} already declares a CRS; drop crs=",
                field.name()
            )))
        }
        (false, Some(crs)) => {
            extension.insert("crs".into(), crs.into());
            extension.remove("crs_type");
        }
        _ => {}
    }
    metadata.insert(
        EXTENSION_METADATA.into(),
        Value::Object(extension).to_string(),
    );
    Ok(field.clone().with_metadata(metadata))
}

/// Stage `reader` to a GeoParquet file named `{stem}.parquet` in a fresh
/// directory under `spill_dir`, rows in stream order.
///
/// The geometry is `geometry_column` (a WKB binary or GeoArrow column), or
/// else every GeoArrow extension column, the first one primary. `crs` labels
/// a stream that declares none; the pipeline's own CRS check then accepts or
/// refuses it as it would a file.
///
/// # Errors
///
/// [`ConvertError::ArrowStream`] for a stream with no usable geometry
/// column; an I/O, Arrow or Parquet error while staging.
pub fn stage_arrow_stream(
    reader: impl RecordBatchReader,
    geometry_column: Option<&str>,
    crs: Option<&str>,
    spill_dir: Option<&Path>,
    stem: &str,
) -> Result<StagedStream, ConvertError> {
    let source = reader.schema();
    let geometry: Vec<usize> = match geometry_column {
        Some(name) => vec![source
            .index_of(name)
            .map_err(|_| stream_error(format!("geometry_column {name:?} is not in the stream")))?],
        None => (0..source.fields().len())
            .filter(|&i| {
                source
                    .field(i)
                    .extension_type_name()
                    .is_some_and(|n| n.starts_with("geoarrow."))
            })
            .collect(),
    };
    let Some(&primary) = geometry.first() else {
        return Err(stream_error(
            "the stream has no GeoArrow geometry column; name its WKB column with geometry_column=",
        ));
    };
    let fields = source
        .fields()
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let f = f.as_ref().clone().with_data_type(unview(f.data_type()));
            if geometry.contains(&i) {
                geometry_field(&f, crs).map(Arc::new)
            } else {
                Ok(Arc::new(f))
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    // Schema-level metadata dropped: a stale `geo` key would sit beside ours.
    let schema = Arc::new(Schema::new(fields));
    // Every WKB column the encoder will parse, selected or not.
    let wkb = crate::resolved_schema::wkb_columns(&schema);

    let options = GeoParquetWriterOptionsBuilder::default()
        .set_encoding(GeoParquetWriterEncoding::WKB)
        .set_primary_column(source.field(primary).name().clone())
        .set_crs_transform(Box::new(KeepCrs))
        .build();
    let mut encoder = GeoParquetRecordBatchEncoder::try_new(&schema, &options)
        .map_err(|e| stream_error(e.to_string()))?;

    let staged = StagedStream(crate::atomic_output::reserve_named(
        ".tylertoo-stream-",
        &format!("{stem}.parquet"),
        spill_dir,
    )?);
    let file = std::fs::File::create_new(staged.path())?;
    let mut writer =
        tylertoo_io::batch_writer(file, encoder.target_schema(), Some(STAGE_ROW_GROUP_ROWS))
            .map_err(ConvertError::Parquet)?;
    let mut rows = 0;
    for batch in reader {
        let batch = batch.map_err(ConvertError::Arrow)?;
        // At most one row group per slice, so the cast and the encode never
        // hold more than that whatever batch size the producer chose.
        for at in (0..batch.num_rows()).step_by(STAGE_ROW_GROUP_ROWS) {
            let slice = batch.slice(at, STAGE_ROW_GROUP_ROWS.min(batch.num_rows() - at));
            let columns = slice
                .columns()
                .iter()
                .zip(schema.fields())
                .map(|(column, field)| arrow_cast::cast(column, field.data_type()))
                .collect::<Result<Vec<_>, _>>()
                .map_err(ConvertError::Arrow)?;
            // The pinned WKB reader panics on some malformed values; refuse
            // them first, as the file readers do.
            for (i, name) in &wkb {
                crate::wkb_column::check_wkb_column(columns[*i].as_ref()).map_err(|(row, e)| {
                    stream_error(format!("geometry column {name:?}, row {}: {e}", rows + row))
                })?;
            }
            rows += slice.num_rows();
            let slice =
                RecordBatch::try_new(schema.clone(), columns).map_err(ConvertError::Arrow)?;
            let encoded = encoder
                .encode_record_batch(&slice)
                .map_err(|e| stream_error(e.to_string()))?;
            writer.write(&encoded).map_err(ConvertError::Parquet)?;
        }
    }
    writer.append_key_value_metadata(
        encoder
            .into_keyvalue()
            .map_err(|e| stream_error(e.to_string()))?,
    );
    writer.close().map_err(ConvertError::Parquet)?;
    Ok(staged)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{BinaryArray, Int32Array, RecordBatchIterator};

    fn point_wkb(x: f64, y: f64) -> Vec<u8> {
        let mut out = vec![1u8, 1, 0, 0, 0];
        out.extend_from_slice(&x.to_le_bytes());
        out.extend_from_slice(&y.to_le_bytes());
        out
    }

    fn stream(rows: usize, geometry: Field) -> impl RecordBatchReader {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            geometry,
        ]));
        let ids = Int32Array::from_iter_values(0..i32::try_from(rows).unwrap());
        let wkb: Vec<Vec<u8>> = (0..rows).map(|i| point_wkb(i as f64 * 1e-4, 0.0)).collect();
        let geoms = BinaryArray::from_iter_values(wkb.iter());
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(ids), Arc::new(geoms)]).unwrap();
        RecordBatchIterator::new(vec![Ok(batch)], schema)
    }

    fn geo_json(path: &Path) -> (usize, Value) {
        let meta = tylertoo_io::read_parquet_metadata(&std::fs::File::open(path).unwrap()).unwrap();
        let geo = meta
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .find(|kv| kv.key == "geo")
            .and_then(|kv| kv.value.clone())
            .unwrap();
        (meta.num_row_groups(), serde_json::from_str(&geo).unwrap())
    }

    #[test]
    fn a_named_wkb_column_stages_with_its_crs_and_capped_row_groups() {
        let spill = tempfile::tempdir().unwrap();
        let rows = STAGE_ROW_GROUP_ROWS + 1;
        let staged = stage_arrow_stream(
            stream(rows, Field::new("wkb", DataType::Binary, true)),
            Some("wkb"),
            Some("EPSG:3857"),
            Some(spill.path()),
            "roads",
        )
        .unwrap();
        assert!(staged.path().starts_with(spill.path()));
        assert_eq!(staged.path().file_name().unwrap(), "roads.parquet");
        let (row_groups, geo) = geo_json(staged.path());
        assert_eq!(row_groups, 2);
        assert_eq!(geo["primary_column"], "wkb");
        assert_eq!(geo["columns"]["wkb"]["crs"], "EPSG:3857");

        let dir = staged.path().parent().unwrap().to_path_buf();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "other users cannot read the stage");
        }
        drop(staged);
        assert!(!dir.exists(), "the staged directory is removed on drop");
    }

    #[test]
    fn a_stream_without_a_crs_stages_an_explicit_null() {
        let geometry = Field::new("geometry", DataType::Binary, true)
            .with_metadata([(EXTENSION_NAME.to_string(), "geoarrow.wkb".to_string())].into());
        let staged = stage_arrow_stream(stream(3, geometry), None, None, None, "x").unwrap();
        let (_, geo) = geo_json(staged.path());
        assert_eq!(geo["columns"]["geometry"]["crs"], Value::Null);
    }

    #[test]
    fn a_crs_on_top_of_a_declared_one_and_a_missing_geometry_are_refused() {
        let declared = Field::new("geometry", DataType::Binary, true).with_metadata(
            [
                (EXTENSION_NAME.to_string(), "geoarrow.wkb".to_string()),
                (
                    EXTENSION_METADATA.to_string(),
                    r#"{"crs":"EPSG:4326"}"#.to_string(),
                ),
            ]
            .into(),
        );
        let err = stage_arrow_stream(stream(1, declared), None, Some("EPSG:3857"), None, "x")
            .unwrap_err();
        assert!(err.to_string().contains("already declares a CRS"), "{err}");

        let plain = Field::new("geometry", DataType::Binary, true);
        let err = stage_arrow_stream(stream(1, plain), None, None, None, "x").unwrap_err();
        assert!(err.to_string().contains("geometry_column="), "{err}");
        assert_eq!(
            crate::error::Classify::kind(&err),
            crate::error::ErrorKind::Input
        );
    }

    /// A MultiPoint whose last member claims an SRID it has no room for
    /// panics the pinned WKB reader; staging refuses it instead.
    #[test]
    fn a_malformed_wkb_value_is_refused_not_a_panic() {
        let mut wkb = vec![1u8, 4, 0, 0, 0, 2, 0, 0, 0];
        wkb.extend_from_slice(&point_wkb(1.0, 2.0));
        wkb.extend_from_slice(&[1, 1, 0, 0, 0x20]);
        wkb.extend_from_slice(&3.0f64.to_le_bytes());
        wkb.extend_from_slice(&4.0f64.to_le_bytes());
        let schema = Arc::new(Schema::new(vec![Field::new("wkb", DataType::Binary, true)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(BinaryArray::from_iter_values([wkb]))],
        )
        .unwrap();
        let reader = RecordBatchIterator::new(vec![Ok(batch.clone())], schema);
        let err = stage_arrow_stream(reader, Some("wkb"), None, None, "x").unwrap_err();
        assert!(err.to_string().contains("row 0"), "{err}");

        // A secondary GeoArrow column is parsed too, so it is checked too.
        let secondary = Field::new("other", DataType::Binary, true)
            .with_metadata([(EXTENSION_NAME.to_string(), "geoarrow.wkb".to_string())].into());
        let schema = Arc::new(Schema::new(vec![
            Field::new("wkb", DataType::Binary, true),
            secondary,
        ]));
        let good = BinaryArray::from_iter_values([point_wkb(0.0, 0.0)]);
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(good), batch.column(0).clone()],
        )
        .unwrap();
        let reader = RecordBatchIterator::new(vec![Ok(batch)], schema);
        let err = stage_arrow_stream(reader, Some("wkb"), None, None, "x").unwrap_err();
        assert!(err.to_string().contains(r#""other", row 0"#), "{err}");
    }
}
