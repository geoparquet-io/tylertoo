//! `tiles --layer-column` (#821): one input, one MVT layer per value of a
//! column. This is tippecanoe's per-feature `tippecanoe.layer`, and what
//! geoparquet-io's `--layer-by-column` sets on each feature.
//!
//! It is a thin layer over pyramid composition (#385/#392). The input is
//! read once and its rows are split by value into per-layer staged
//! GeoParquet files. Each file then runs the ordinary `tiles` chain with the
//! caller's options, and [`crate::pyramid::merge_bands_with_options`]
//! concatenates the layers tile by tile. So each layer has its own ladder:
//! the density budget, thinning, coalescing and the per-tile size valve run
//! per layer, as for separate tippecanoe inputs. Each layer is generalized
//! independently, so the overview spec's one-dataset-per-file rule holds.
//!
//! - A null value, or one that is blank after trimming, goes to
//!   [`UNKNOWN_LAYER`]. Any other value is trimmed and used as the layer name.
//!   This matches gpio's tippecanoe pipeline.
//! - The column decides the layer and is not written as a property;
//!   tippecanoe does not write `tippecanoe.layer` either.
//! - DIVERGENCE FROM TIPPECANOE: the tile size caps apply to each layer
//!   before the layers are joined, not to the combined tile. The merge
//!   concatenates encoded layers and has no features left to shed.
//! - At most [`MAX_LAYERS`] layers.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use arrow_array::{Array, RecordBatch, StringArray, UInt32Array};
use arrow_schema::{DataType, SchemaRef};
use serde::Serialize;

use super::convert::ConvertError;
use super::tiles::{TilesError, TilesInput, TilesOptions, TilesOutcome, TilesReport};
use crate::atomic_output::{PendingOutput, Reserved};
use crate::input::InputError;
use crate::input_set::{ConvertSource, ReadPlan};
use crate::pyramid::{Band, BandMergeOptions};

/// The layer a null or blank value goes to.
pub const UNKNOWN_LAYER: &str = "_unknown";

/// The most layers one `--layer-column` run builds.
///
/// Each layer is a full convert and export with its own temporary overview
/// and archive, and the split holds one open writer per layer. Even a fine
/// categorical split (land-cover classes, admin levels, years) stays well
/// under 64. A column with more values than that is probably an id or a
/// free-text field, and building thousands of layers would take hours
/// before anything failed. Subset the input with `--filter`, or route by a
/// coarser column.
pub const MAX_LAYERS: usize = 64;

/// Why a `--layer-column` split could not run.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LayerColumnError {
    /// The column is not in the input.
    #[error("--layer-column {name:?} is not a column of the input (columns: {available})")]
    #[non_exhaustive]
    MissingColumn {
        /// The requested column.
        name: String,
        /// The input's columns, comma-separated.
        available: String,
    },
    /// The column's type cannot name a layer.
    #[error(
        "--layer-column {name:?} is {data_type}; a layer column must hold strings, integers \
         or booleans"
    )]
    #[non_exhaustive]
    UnsupportedType {
        /// The column.
        name: String,
        /// Its Arrow type.
        data_type: String,
    },
    /// The column has more distinct values than [`MAX_LAYERS`].
    #[error(
        "--layer-column {name:?} has more than {limit} distinct values; each value becomes a \
         layer with its own convert and export. Route by a coarser column, or subset the \
         input with --filter"
    )]
    #[non_exhaustive]
    TooManyLayers {
        /// The column.
        name: String,
        /// [`MAX_LAYERS`].
        limit: usize,
    },
    /// Reading the input failed.
    #[error(transparent)]
    Input(#[from] InputError),
    /// Writing a staged layer file failed.
    #[error("failed to stage the rows of layer {layer:?} in {}: {message}", dir.display())]
    #[non_exhaustive]
    Stage {
        /// The layer being written.
        layer: String,
        /// Where the staged files go.
        dir: std::path::PathBuf,
        /// The underlying error.
        message: String,
    },
}

impl crate::error::Classify for LayerColumnError {
    fn kind(&self) -> crate::error::ErrorKind {
        match self {
            Self::MissingColumn { .. }
            | Self::UnsupportedType { .. }
            | Self::TooManyLayers { .. } => crate::error::ErrorKind::Input,
            Self::Input(e) => e.kind(),
            Self::Stage { .. } => crate::error::ErrorKind::Runtime,
        }
    }
}

/// One layer of a `--layer-column` run: its name, how many input rows it
/// took, and its own convert and export reports. Row numbers in those
/// reports count that layer's rows, in input order.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct LayerReport {
    /// The MVT layer name.
    pub layer: String,
    /// Input rows routed to this layer, after `--filter`.
    pub input_rows: u64,
    /// The layer's convert and export.
    #[serde(flatten)]
    pub report: TilesReport,
}

/// What a `--layer-column` run produced: one report per layer, in layer
/// name order, and the merged archive's totals.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct LayeredReport {
    /// The column the layers were routed by.
    pub layer_column: String,
    /// Every layer that produced tiles.
    pub layers: Vec<LayerReport>,
    /// Layers whose features all generalized or filtered away, with no
    /// tiles to merge.
    pub empty_layers: Vec<String>,
    /// Input rows `--filter` or `--bbox` dropped before the split, in no
    /// layer.
    pub filtered_rows: u64,
    /// Distinct tiles in the merged archive.
    pub total_tiles: usize,
    /// A multipart input's schema warnings (each layer reads a copy of the
    /// first part's footer), then the merge's own diagnostics.
    pub diagnostics: Vec<crate::diagnostics::Diagnostic>,
}

/// The layer `value` routes to: [`UNKNOWN_LAYER`] for null or blank, else
/// the trimmed value.
fn layer_name(value: Option<&str>) -> &str {
    match value.map(str::trim) {
        Some(v) if !v.is_empty() => v,
        _ => UNKNOWN_LAYER,
    }
}

/// Whether a column of type `dt` can name layers.
fn routable(dt: &DataType) -> bool {
    match dt {
        // An all-null column (Arrow `Null`) routes every row to `_unknown`.
        DataType::Null
        | DataType::Utf8
        | DataType::LargeUtf8
        | DataType::Utf8View
        | DataType::Boolean => true,
        DataType::Dictionary(_, value) => routable(value),
        dt => dt.is_integer(),
    }
}

/// Narrow `source` to the columns a layer can use, and return the layers'
/// property selection: the caller's, without the routing column.
///
/// The default selection stages every column. Otherwise only the kept
/// properties are staged, with the geometry, its covering, the routing
/// column and what `filter` reads, and each layer includes exactly the
/// properties the caller kept.
fn stage_columns(
    source: &ConvertSource,
    selection: &super::properties::PropertySelection,
    routing: usize,
    filter: Option<&super::filter::BoundFilter>,
) -> Result<super::properties::PropertySelection, ConvertError> {
    let schema = source.file_schema()?;
    let column = schema.field(routing).name();
    let mut layer = selection.clone();
    if selection.is_identity() {
        layer.exclude.push(column.clone());
        return Ok(layer);
    }
    let geom = source.resolved_schema()?.geometry_index().ok_or_else(|| {
        ConvertError::NoGeometryColumn {
            input: source.display_name(),
        }
    })?;
    let kept = selection.resolve(&schema, geom, &[], &mut |m| log::warn!("[tiles] {m}"))?;
    let properties = super::properties::property_indices(&schema, geom);
    let mut keep: Vec<usize> = (0..schema.fields().len())
        .filter(|i| kept.contains(i) || !properties.contains(i))
        .chain(std::iter::once(routing))
        .chain(filter.map_or(&[][..], |f| f.columns()).iter().copied())
        .collect();
    keep.sort_unstable();
    keep.dedup();
    source.restrict_columns(keep)?;
    layer.include = Some(
        kept.iter()
            .filter(|&&i| i != geom && i != routing)
            .map(|&i| schema.field(i).name().clone())
            .collect(),
    );
    layer.exclude.clear();
    layer.exclude_all = false;
    Ok(layer)
}

/// Refuse a knob that reads the routing column, which no layer keeps.
pub(super) fn check_knobs(
    convert: &super::convert::ConvertOptions,
    export: &super::export::ExportOptions,
    column: &str,
    schema: Option<&arrow_schema::Schema>,
) -> Result<(), super::config::ConfigError> {
    let mut knobs = schema
        .map(|s| super::convert::knob_columns(convert, s))
        .unwrap_or_default();
    if let super::export::FeatureOrder::Column { name, .. } = &export.feature_order {
        knobs.push((name.clone(), "--feature-order".to_string()));
    }
    if let Some(name) = &export.feature_id {
        knobs.push((name.clone(), "--feature-id".to_string()));
    }
    match knobs.into_iter().find(|(name, _)| name == column) {
        Some((name, knob)) => Err(super::config::ConfigError::LayerColumnReadByKnob { name, knob }),
        None => Ok(()),
    }
}

/// A layer's staged GeoParquet file while the split writes it.
struct StagedWriter {
    path: Reserved,
    pending: PendingOutput,
    writer: tylertoo_io::BatchWriter<std::fs::File>,
    rows: u64,
}

/// A layer's finished staged file.
struct Staged {
    name: String,
    path: Reserved,
    rows: u64,
}

/// What decides, before the split, which input rows reach a layer.
struct Gates {
    filter: Option<super::filter::BoundFilter>,
    bbox: Option<super::convert::BboxQuery>,
    /// The row groups whose footer statistics let the two through.
    row_groups: Option<crate::input_set::RowGroupSelection>,
}

impl Gates {
    /// `--filter` and `--bbox`, bound to the staged `schema`. They run once,
    /// before the split, so the filter may read the routing column and the
    /// layer limit counts only the rows they keep. The layers keep the bbox,
    /// which then passes every row.
    fn new(
        source: &ConvertSource,
        schema: &arrow_schema::Schema,
        options: &super::convert::ConvertOptions,
    ) -> Result<Self, ConvertError> {
        let filter = super::convert::bind_attribute_filter(options, schema, &[])?;
        let crs = source.resolved_schema()?.crs();
        let bbox = options
            .bbox
            .zip(crs)
            .map(|(b, crs)| super::convert::BboxQuery::new(&b, crs));
        let row_groups = super::stream::select_row_groups_streaming(
            source,
            bbox.as_ref(),
            filter.as_ref(),
            None,
            // Only a shard's bounds read the CRS, and there is no shard.
            crs.unwrap_or(super::level::Crs::Epsg4326),
        )?;
        Ok(Self {
            filter,
            bbox,
            row_groups,
        })
    }
}

/// The input's GeoParquet version verdict over all its parts, and a
/// multipart input's schema warnings. A staged file carries the first
/// part's footer, so its layers would report that part's alone.
fn provenance(
    source: &ConvertSource,
) -> Result<
    (
        crate::resolved_schema::GeoVersion,
        Vec<crate::diagnostics::Diagnostic>,
    ),
    InputError,
> {
    let diagnostics = crate::diagnostics::Diagnostics::default();
    source.record_diagnostics(&diagnostics);
    let version = crate::resolved_schema::GeoVersion::of_parts(
        source
            .resolved_schemas()?
            .iter()
            .map(|r| r.declared_version()),
    );
    Ok((version, diagnostics.snapshot()))
}

/// Read `source` once and write each layer's rows that the filter and the
/// bbox keep, every column included, to its own GeoParquet file in `dir`.
/// Layers come back in name order, with the number of rows the two dropped.
/// A row whose geometry has no envelope (null, empty) is kept, for its
/// layer to account for.
///
/// Every writer's buffered row group counts against the staging memory
/// budget ([`crate::input::unbudgeted_stage_admission`]); past it, the
/// largest buffer is flushed as a row group.
fn split(
    source: &ConvertSource,
    schema: &SchemaRef,
    idx: usize,
    gates: &Gates,
    dir: &Path,
    batch_size: usize,
) -> Result<(Vec<Staged>, u64), LayerColumnError> {
    let field = schema.field(idx);
    let column = field.name().as_str();
    let metadata = staged_metadata(source)?;
    let budget = crate::input::unbudgeted_stage_admission() as usize;
    let stage_err = |layer: &str, e: &dyn std::fmt::Display| LayerColumnError::Stage {
        layer: layer.to_string(),
        dir: dir.to_path_buf(),
        message: e.to_string(),
    };

    let (filter, bbox) = (gates.filter.as_ref(), gates.bbox.as_ref());
    // The bbox's geometry column, restated in the staged schema.
    let geom = match bbox {
        Some(_) => source
            .resolved_schema()?
            .geometry_name()
            .and_then(|name| schema.index_of(name).ok()),
        None => None,
    };
    let row_groups = gates.row_groups.as_ref();

    let mut layers: BTreeMap<String, StagedWriter> = BTreeMap::new();
    // The rows of the row groups the gates prune count as dropped too.
    let pruned = source.selected_row_count(None)? - source.selected_row_count(row_groups)?;
    let mut filtered = u64::try_from(pruned).unwrap_or(0);
    let stream = source.open_stream(&ReadPlan {
        batch_size,
        projection: None,
        row_groups,
    })?;
    for batch in stream {
        let batch = batch?;
        // A multi-part source's parts may differ in nullability; write
        // every part under the union schema the source validated.
        let batch = RecordBatch::try_new(Arc::clone(schema), batch.columns().to_vec())
            .map_err(|e| stage_err(column, &e))?;
        let keep = filter.map(|f| f.eval_mask(&batch, &|i| i));
        let outside = match (bbox, geom) {
            (Some(query), Some(g)) => {
                outside_bbox(&batch, g, query).map_err(|e| stage_err(column, &e))?
            }
            _ => Vec::new(),
        };
        let values = arrow_cast::cast(batch.column(idx), &DataType::Utf8)
            .map_err(|e| stage_err(column, &e))?;
        let values = values
            .as_any()
            .downcast_ref::<StringArray>()
            .expect("cast to Utf8 yields a StringArray");
        let mut rows: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
        for i in 0..values.len() {
            // SQL semantics: a row the filter cannot decide is dropped.
            if keep.as_ref().is_some_and(|k| k[i] != Some(true)) || outside.get(i) == Some(&true) {
                filtered += 1;
                continue;
            }
            let value = values.is_valid(i).then(|| values.value(i));
            rows.entry(layer_name(value)).or_default().push(i as u32);
        }
        for (name, take) in rows {
            if !layers.contains_key(name) {
                if layers.len() == MAX_LAYERS {
                    return Err(LayerColumnError::TooManyLayers {
                        name: field.name().clone(),
                        limit: MAX_LAYERS,
                    });
                }
                let staged =
                    open_staged(dir, schema, &metadata).map_err(|e| stage_err(name, &e))?;
                layers.insert(name.to_string(), staged);
            }
            let staged = layers.get_mut(name).expect("inserted above");
            let part = arrow_select::take::take_record_batch(&batch, &UInt32Array::from(take))
                .map_err(|e| stage_err(name, &e))?;
            staged.rows += part.num_rows() as u64;
            staged
                .writer
                .write(&part)
                .map_err(|e| stage_err(name, &e))?;
        }
        flush_over_budget(&mut layers, budget).map_err(|(name, e)| stage_err(&name, &e))?;
    }

    let staged = layers
        .into_iter()
        .map(|(name, staged)| {
            staged.writer.close().map_err(|e| stage_err(&name, &e))?;
            staged.pending.publish().map_err(|e| stage_err(&name, &e))?;
            Ok(Staged {
                name,
                path: staged.path,
                rows: staged.rows,
            })
        })
        .collect::<Result<Vec<_>, LayerColumnError>>()?;
    Ok((staged, filtered))
}

/// The index of the routing column in `schema`, refusing a missing column or
/// one whose values cannot name a layer.
fn routing_column(schema: &arrow_schema::Schema, column: &str) -> Result<usize, LayerColumnError> {
    let idx = super::filter::resolve_column(schema, column).ok_or_else(|| {
        LayerColumnError::MissingColumn {
            name: column.to_string(),
            available: schema
                .fields()
                .iter()
                .map(|f| f.name().as_str())
                .collect::<Vec<_>>()
                .join(", "),
        }
    })?;
    let field = schema.field(idx);
    if !routable(field.data_type()) {
        return Err(LayerColumnError::UnsupportedType {
            name: field.name().clone(),
            data_type: field.data_type().to_string(),
        });
    }
    Ok(idx)
}

/// Per row of `batch`, whether its geometry (column `geom`) has an envelope
/// that misses `query`: what convert drops as outside `--bbox`.
fn outside_bbox(
    batch: &RecordBatch,
    geom: usize,
    query: &super::convert::BboxQuery,
) -> crate::Result<Vec<bool>> {
    let rows: Vec<usize> = (0..batch.num_rows()).collect();
    let field = batch.schema_ref().field(geom).clone();
    let array = geoarrow::array::from_arrow_array(batch.column(geom).as_ref(), &field)
        .map_err(|e| crate::GeoParquetReadError::decode("--layer-column bbox", e))?;
    let envelopes = crate::batch_processor::visit_geoarrow_array(
        array.as_ref(),
        super::auto_zoom::SampledBboxes { rows: &rows },
    )?;
    Ok(envelopes
        .iter()
        .map(|b| b.is_some_and(|b| !query.intersects(&b)))
        .collect())
}

/// The footer metadata of every staged file, so that it reads like the
/// input: the input's own (the `geo` block above all), less the writer's
/// Arrow schema and the input's schema descriptor, which a property
/// selection makes stale (the reader derives one when it is absent).
///
/// A CRS that only a native `GEOMETRY` type declares (no `geo` key, or one
/// without the geometry column) is restated in the `geo` block, since the
/// staging writer cannot emit that type. Its identifier is copied verbatim:
/// the reader parses a `geo` string `crs` the way it parses the native one,
/// so the layers resolve, or refuse, the same CRS.
fn staged_metadata(source: &ConvertSource) -> Result<Vec<tylertoo_io::KeyValue>, InputError> {
    use serde_json::{json, Value};

    let mut metadata: Vec<_> = source
        .key_value_metadata()?
        .unwrap_or_default()
        .into_iter()
        .filter(|kv| {
            kv.key != "ARROW:schema" && kv.key != crate::resolved_schema::RESOLVED_SCHEMA_KEY
        })
        .collect();
    let resolved = source.resolved_schema()?;
    let crs = resolved.crs_info();
    let Some(name) = resolved.geometry_name().filter(|_| !crs.assumed_wgs84) else {
        return Ok(metadata);
    };
    let at = metadata
        .iter()
        .position(|kv| kv.key.eq_ignore_ascii_case("geo"));
    let mut geo = match at.map(|i| metadata[i].value.as_deref().map(serde_json::from_str)) {
        None => json!({ "version": "1.1.0" }),
        Some(Some(Ok(geo @ Value::Object(_)))) => geo,
        // Unreadable: the input's own read settled what it means.
        Some(_) => return Ok(metadata),
    };
    if geo.get("columns").and_then(|c| c.get(name)).is_some() {
        return Ok(metadata);
    }
    if !geo.get("columns").is_some_and(Value::is_object) {
        geo["columns"] = json!({});
    }
    let mut column = json!({ "encoding": "WKB", "geometry_types": [] });
    if let Some(id) = &crs.identifier {
        column["crs"] = id.as_str().into();
    }
    geo["columns"][name] = column;
    if geo.get("primary_column").is_none() {
        geo["primary_column"] = name.into();
    }
    let geo = tylertoo_io::KeyValue::new("geo".to_string(), geo.to_string());
    match at {
        Some(i) => metadata[i] = geo,
        None => metadata.push(geo),
    }
    Ok(metadata)
}

/// Create one layer's staged file under `dir`.
fn open_staged(
    dir: &Path,
    schema: &SchemaRef,
    metadata: &[tylertoo_io::KeyValue],
) -> Result<StagedWriter, Box<dyn std::error::Error>> {
    let path = crate::atomic_output::reserve_path(".tylertoo-layer-", ".parquet", Some(dir))?;
    let (file, pending) = crate::atomic_output::create(&path)?;
    let mut writer = tylertoo_io::batch_writer(file, Arc::clone(schema))?;
    for kv in metadata {
        writer.append_key_value_metadata(kv.clone());
    }
    Ok(StagedWriter {
        path,
        pending,
        writer,
        rows: 0,
    })
}

/// Flush the largest buffered row groups until all of them fit `budget`.
fn flush_over_budget(
    layers: &mut BTreeMap<String, StagedWriter>,
    budget: usize,
) -> Result<(), (String, tylertoo_io::ParquetError)> {
    loop {
        let total: usize = layers.values().map(|s| s.writer.in_progress_size()).sum();
        if total <= budget {
            return Ok(());
        }
        let Some((name, staged)) = layers
            .iter_mut()
            .max_by_key(|(_, s)| s.writer.in_progress_size())
        else {
            return Ok(());
        };
        if staged.writer.in_progress_size() == 0 {
            return Ok(());
        }
        staged.writer.flush().map_err(|e| (name.clone(), e))?;
    }
}

/// The `--layer-column` run of [`super::tiles::convert_to_pmtiles_observed`]:
/// split, tile each layer, merge. `options` has been validated.
///
/// The layers' intermediate overviews are not reported to an observer: they
/// are temporary per layer, and `--keep-overview` is refused here.
pub(super) fn convert_by_layer(
    input: &TilesInput,
    output: &Path,
    options: &TilesOptions,
    column: &str,
) -> Result<TilesOutcome, TilesError> {
    crate::atomic_output::check_writable(output)?;
    let source = input.source().map_err(TilesError::Convert)?;
    // One zoom range for every layer: `--max-zoom auto` is estimated over
    // the whole input, as tippecanoe's `-zg` is.
    let convert = super::tiles::resolve_on_source(
        &source,
        None,
        std::borrow::Cow::Borrowed(&options.convert),
    )?
    .into_owned();
    let file_schema = source
        .file_schema()
        .map_err(|e| TilesError::Convert(e.into()))?;
    let idx = routing_column(&file_schema, column)?;
    // The schema's own spelling: the property selection matches names exactly.
    let column = file_schema.field(idx).name().as_str();
    // The filter runs once, here, so it may read the routing column and the
    // layer limit counts only the rows it keeps; the layers skip it.
    let filter = super::convert::bind_attribute_filter(&convert, &file_schema, &[])
        .map_err(TilesError::Convert)?;
    let mut convert = convert;
    convert.filter = None;
    check_knobs(&convert, &options.export, column, Some(&file_schema))?;
    convert.properties = stage_columns(&source, &convert.properties, idx, filter.as_ref())
        .map_err(TilesError::Convert)?;
    let schema = source.schema().map_err(|e| TilesError::Convert(e.into()))?;
    let idx = schema
        .index_of(column)
        .expect("the routing column is staged");
    let gates = Gates::new(&source, &schema, &options.convert).map_err(TilesError::Convert)?;
    let (geo_version, input_diagnostics) =
        provenance(&source).map_err(|e| TilesError::Convert(e.into()))?;

    let dir = super::tiles::intermediate_dir(options, output);
    let (staged, filtered_rows) = split(
        &source,
        &schema,
        idx,
        &gates,
        &dir,
        options.convert.read_batch_size,
    )?;
    drop(source);
    log::info!(
        "[tiles] --layer-column {column:?}: {} layer(s): {}",
        staged.len(),
        staged
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let layer_convert = convert;
    let mut layers = Vec::new();
    let mut empty_layers = Vec::new();
    let mut archives: Vec<(Band, Reserved)> = Vec::new();
    for layer in staged {
        let archive =
            crate::atomic_output::reserve_path(".tylertoo-layer-", ".pmtiles", Some(&dir))
                .map_err(|source| TilesError::IntermediateCreate {
                    dir: dir.clone(),
                    source,
                })?;
        let layer_options = TilesOptions {
            convert: layer_convert.clone(),
            export: super::export::ExportOptions {
                // The merged archive's metadata is set once, by the merge.
                metadata: crate::pmtiles_metadata::TilesetMetadata::default(),
                ..options.export.clone()
            },
            layer_name: Some(layer.name.clone()),
            layer_column: None,
            ..options.clone()
        };
        let config = layer_options.validate()?;
        let staged_input = TilesInput::Path(layer.path.to_path_buf());
        let in_layer = |e| TilesError::Layer {
            layer: layer.name.clone(),
            source: Box::new(e),
        };
        let mut report = match super::tiles::convert_to_pmtiles_validated(
            &staged_input,
            || staged_input.source(),
            &archive,
            &config,
            &mut (),
        ) {
            Ok(TilesOutcome::Converted(report)) => report,
            Err(TilesError::Convert(e)) if super::tiles::convert_produced_nothing(&e) => {
                log::warn!(
                    "[tiles] layer {:?} has nothing to draw ({e}); left out",
                    layer.name
                );
                empty_layers.push(layer.name);
                continue;
            }
            // Only a shard job ends empty without an error, and
            // `--layer-column` refuses `--shard`.
            Ok(other) => {
                return Err(in_layer(TilesError::Convert(ConvertError::Io(
                    std::io::Error::other(format!("unexpected outcome {other:?}")),
                ))))
            }
            Err(e) => return Err(in_layer(e)),
        };
        if report.export.total_tiles == 0 {
            log::warn!("[tiles] layer {:?} wrote no tiles; left out", layer.name);
            empty_layers.push(layer.name);
            continue;
        }
        archives.push((
            Band::new(
                archive.to_path_buf(),
                layer.name.clone(),
                report.export.min_zoom,
                report.export.max_zoom,
            ),
            archive,
        ));
        report.convert.geo_version = geo_version.clone();
        layers.push(LayerReport {
            layer: layer.name,
            input_rows: layer.rows,
            report: *report,
        });
    }
    if archives.is_empty() {
        return Err(TilesError::Convert(ConvertError::NoData));
    }
    let bands: Vec<Band> = archives.iter().map(|(band, _)| band.clone()).collect();
    let merged = crate::pyramid::merge_bands_with_options(
        &bands,
        output,
        &BandMergeOptions {
            allow_missing_zooms: false,
            metadata: options.export.metadata.clone(),
            // The layers' archives are spooled there already.
            spill_dir: Some(dir.clone()),
        },
    )
    .map_err(TilesError::Merge)?;
    Ok(TilesOutcome::Layered(Box::new(LayeredReport {
        layer_column: column.to_string(),
        layers,
        empty_layers,
        filtered_rows,
        total_tiles: merged.total_tiles,
        diagnostics: input_diagnostics
            .into_iter()
            .chain(merged.diagnostics)
            .collect(),
    })))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use prost::Message;

    use super::*;
    use crate::archive_index::ArchiveIndex;
    use crate::compression::{self, Compression};
    use crate::overview::convert::LevelPlan;
    use crate::overview::testutil::{write_multi_rg_attr_input, AttrRow};
    use crate::overview::tiles::convert_to_pmtiles;

    /// Per layer, the features of every tile at `zoom`, each as its tag keys.
    fn layers_at(path: &Path, zoom: u8) -> BTreeMap<String, Vec<Vec<String>>> {
        let index = ArchiveIndex::open(path).unwrap();
        let mut out: BTreeMap<String, Vec<Vec<String>>> = BTreeMap::new();
        for tile in index.tiles() {
            let tile = tile.unwrap();
            if tile.z != zoom {
                continue;
            }
            let raw = index.read_range(tile.range).unwrap();
            let plain = compression::decompress_capped(&raw, Compression::Gzip, 1 << 26).unwrap();
            for layer in crate::vector_tile::Tile::decode(plain.as_slice())
                .unwrap()
                .layers
            {
                let features = layer
                    .features
                    .iter()
                    .map(|f| {
                        f.tags
                            .chunks(2)
                            .map(|kv| layer.keys[kv[0] as usize].clone())
                            .collect()
                    })
                    .collect::<Vec<Vec<String>>>();
                out.entry(layer.name).or_default().extend(features);
            }
        }
        out
    }

    fn rows() -> Vec<AttrRow> {
        vec![
            ((10.0, 10.0), Some(0.5), Some("soy")),
            ((-20.0, 5.0), Some(0.5), Some("corn")),
            ((30.0, -15.0), Some(0.5), None),
            ((40.0, 20.0), Some(0.5), Some("   ")),
            ((-50.0, -30.0), Some(0.5), Some(" soy ")),
            ((60.0, 45.0), Some(0.5), Some("a:b")),
        ]
    }

    fn options(column: &str) -> TilesOptions {
        TilesOptions {
            convert: crate::overview::convert::ConvertOptions {
                levels: LevelPlan::ZoomRange {
                    min_zoom: 0,
                    max_zoom: 3,
                },
                ..TilesOptions::default().convert
            },
            layer_column: Some(column.to_string()),
            ..TilesOptions::default()
        }
    }

    /// #821: each value lands in its own layer, null and blank in
    /// `_unknown`, the routing column is not a property, and the metadata
    /// lists every layer.
    #[test]
    fn routes_each_value_to_its_own_layer() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("crops.parquet");
        write_multi_rg_attr_input(&input, &rows());
        let out = dir.path().join("crops.pmtiles");

        let outcome = convert_to_pmtiles(&TilesInput::Path(input), &out, &options("crop")).unwrap();
        let TilesOutcome::Layered(report) = outcome else {
            panic!("expected a layered outcome, got {outcome:?}");
        };
        let names: Vec<&str> = report.layers.iter().map(|l| l.layer.as_str()).collect();
        assert_eq!(names, ["_unknown", "a:b", "corn", "soy"]);
        let rows: Vec<u64> = report.layers.iter().map(|l| l.input_rows).collect();
        assert_eq!(rows, [2, 1, 1, 2]);

        let layers = layers_at(&out, 3);
        let counts: BTreeMap<&str, usize> =
            layers.iter().map(|(k, v)| (k.as_str(), v.len())).collect();
        assert_eq!(
            counts,
            BTreeMap::from([("_unknown", 2), ("a:b", 1), ("corn", 1), ("soy", 2)])
        );
        for features in layers.values() {
            for keys in features {
                assert!(
                    !keys.iter().any(|k| k == "crop"),
                    "routing column emitted: {keys:?}"
                );
                assert!(
                    keys.iter().any(|k| k == "confidence"),
                    "properties kept: {keys:?}"
                );
            }
        }

        let meta = ArchiveIndex::open(&out).unwrap().metadata_json().unwrap();
        let ids: BTreeSet<&str> = meta["vector_layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, BTreeSet::from(["_unknown", "a:b", "corn", "soy"]));
        for layer in meta["vector_layers"].as_array().unwrap() {
            assert!(layer["fields"].get("crop").is_none(), "{layer}");
            assert_eq!(layer["maxzoom"], 3, "{layer}");
        }
        let stats: BTreeSet<&str> = meta["tilestats"]["layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["layer"].as_str().unwrap())
            .collect();
        assert_eq!(stats, ids);
        assert_eq!(meta["tilestats"]["layerCount"], 4);

        // The staged inputs, overviews and per-layer archives are gone.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 2, "left behind: {left:?}");
    }

    fn refusal(mut options: TilesOptions, edit: impl FnOnce(&mut TilesOptions)) -> String {
        edit(&mut options);
        options.validate().unwrap_err().to_string()
    }

    #[test]
    fn refuses_options_a_layer_run_cannot_honour() {
        let base = options("crop");
        type Edit = Box<dyn FnOnce(&mut TilesOptions)>;
        let cases: [(&str, Edit); 6] = [
            (
                "--layer-name",
                Box::new(|o| o.layer_name = Some("x".into())),
            ),
            (
                "--shard",
                Box::new(|o| {
                    o.shard = Some(crate::overview::tiles::TilesShard::new(
                        crate::shard::ShardRole::Coarse,
                        None,
                        2,
                        "0",
                        None,
                    ))
                }),
            ),
            (
                "--save-plan",
                Box::new(|o| o.convert.save_plan = Some("p".into())),
            ),
            (
                "--keep-overview",
                Box::new(|o| o.keep_overview = Some("k.parquet".into())),
            ),
            (
                "--generate-variable-depth-tile-pyramid",
                Box::new(|o| o.export.variable_depth = true),
            ),
            (
                "--feature-id",
                Box::new(|o| o.export.feature_id = Some("crop".into())),
            ),
        ];
        for (flag, edit) in cases {
            let message = refusal(base.clone(), edit);
            assert!(message.contains("--layer-column"), "{flag}: {message}");
            assert!(message.contains(flag), "{flag}: {message}");
        }
    }

    /// The filter runs once, before the split: it may read the routing
    /// column, and the column's case-insensitive spelling still keeps it out
    /// of the properties.
    #[test]
    fn filters_before_the_split_and_resolves_the_column_once() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("crops.parquet");
        write_multi_rg_attr_input(&input, &rows());
        let out = dir.path().join("o.pmtiles");
        let mut options = options("CROP");
        options.convert.filter = Some("crop != 'corn'".into());
        let outcome = convert_to_pmtiles(&TilesInput::Path(input), &out, &options).unwrap();
        let TilesOutcome::Layered(report) = outcome else {
            panic!("expected a layered outcome, got {outcome:?}");
        };
        assert_eq!(report.layer_column, "crop");
        // The null row is neither kept nor dropped by `!=`: SQL drops it.
        let rows: Vec<(&str, u64)> = report
            .layers
            .iter()
            .map(|l| (l.layer.as_str(), l.input_rows))
            .collect();
        assert_eq!(rows, [("_unknown", 1), ("a:b", 1), ("soy", 2)]);
        assert_eq!(report.filtered_rows, 2);
        for features in layers_at(&out, 3).values() {
            for keys in features {
                assert!(!keys.iter().any(|k| k == "crop"), "{keys:?}");
            }
        }
    }

    #[test]
    fn refuses_a_missing_or_unroutable_column() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("crops.parquet");
        write_multi_rg_attr_input(&input, &rows());
        let out = dir.path().join("o.pmtiles");
        let input = TilesInput::Path(input);

        let err = convert_to_pmtiles(&input, &out, &options("nope")).unwrap_err();
        assert!(
            err.to_string().contains("\"nope\" is not a column"),
            "{err}"
        );
        assert_eq!(
            crate::error::Classify::kind(&err),
            crate::error::ErrorKind::Input
        );

        let err = convert_to_pmtiles(&input, &out, &options("confidence")).unwrap_err();
        assert!(
            err.to_string().contains("strings, integers or booleans"),
            "{err}"
        );
    }

    #[test]
    fn refuses_more_layers_than_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("many.parquet");
        let names: Vec<&'static str> = (0..=MAX_LAYERS)
            .map(|i| &*Box::leak(format!("v{i}").into_boxed_str()))
            .collect();
        let rows: Vec<AttrRow> = names
            .iter()
            .enumerate()
            .map(|(i, n)| ((i as f64 - 30.0, 0.0), None, Some(*n)))
            .collect();
        write_multi_rg_attr_input(&input, &rows);
        let input = TilesInput::Path(input);
        let err = convert_to_pmtiles(&input, &dir.path().join("o.pmtiles"), &options("crop"))
            .unwrap_err();
        assert!(
            err.to_string().contains(&format!("more than {MAX_LAYERS}")),
            "{err}"
        );

        // The limit counts the layers the filter keeps.
        let mut options = options("crop");
        options.convert.filter = Some("crop = 'v3'".into());
        let outcome = convert_to_pmtiles(&input, &dir.path().join("o.pmtiles"), &options).unwrap();
        let TilesOutcome::Layered(report) = outcome else {
            panic!("expected a layered outcome, got {outcome:?}");
        };
        assert_eq!(report.layers.len(), 1);
        assert_eq!(report.filtered_rows, MAX_LAYERS as u64);

        // And the layers `--bbox` keeps: v0..v4 lie at x = -30..-26.
        let mut boxed = super::tests::options("crop");
        boxed.convert.bbox = Some([-30.5, -1.0, -25.5, 1.0]);
        let outcome = convert_to_pmtiles(&input, &dir.path().join("o.pmtiles"), &boxed).unwrap();
        let TilesOutcome::Layered(report) = outcome else {
            panic!("expected a layered outcome, got {outcome:?}");
        };
        let names: Vec<&str> = report.layers.iter().map(|l| l.layer.as_str()).collect();
        assert_eq!(names, ["v0", "v1", "v2", "v3", "v4"]);
        assert_eq!(report.filtered_rows, MAX_LAYERS as u64 + 1 - 5);
    }

    /// A property selection stages only what a layer can use: a filter may
    /// read an excluded column, which no layer then writes.
    #[test]
    fn stages_the_selection_and_what_the_filter_reads() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("crops.parquet");
        write_multi_rg_attr_input(&input, &rows());
        let out = dir.path().join("o.pmtiles");
        let mut options = options("crop");
        options.convert.properties.exclude = vec!["confidence".into()];
        options.convert.filter = Some("confidence > 0.4 AND crop = 'soy'".into());
        let outcome = convert_to_pmtiles(&TilesInput::Path(input), &out, &options).unwrap();
        let TilesOutcome::Layered(report) = outcome else {
            panic!("expected a layered outcome, got {outcome:?}");
        };
        let rows: Vec<(&str, u64)> = report
            .layers
            .iter()
            .map(|l| (l.layer.as_str(), l.input_rows))
            .collect();
        assert_eq!(rows, [("soy", 1)]);
        for features in layers_at(&out, 3).values() {
            for keys in features {
                assert_eq!(keys, &["id"], "only the kept property");
            }
        }
    }

    /// A GeoParquet 2.0 input: a native `GEOMETRY(crs)` column and the
    /// footer entries `metadata` (none: no `geo` key).
    /// Hand-assembled, since this workspace's `ArrowWriter` cannot emit the
    /// native type.
    fn write_native_input(
        path: &Path,
        crs: &str,
        rows: &[(f64, f64, &str)],
        metadata: &[(&str, String)],
    ) {
        use arrow_array::{BinaryArray, RecordBatch, StringArray};
        use arrow_schema::{Field, Schema};
        use tylertoo_io::test_support::{Type, WriterProperties};
        use tylertoo_io::{compute_leaves, ArrowRowGroupWriterFactory, LogicalType, PhysicalType};

        let wkb = rows.iter().map(|&(x, y, _)| {
            [
                &[1u8][..],
                &1u32.to_le_bytes(),
                &x.to_le_bytes(),
                &y.to_le_bytes(),
            ]
            .concat()
        });
        let schema = Arc::new(Schema::new(vec![
            Field::new("geometry", DataType::Binary, true),
            Field::new("crop", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            Arc::clone(&schema),
            vec![
                Arc::new(BinaryArray::from_iter_values(wkb)),
                Arc::new(StringArray::from_iter_values(rows.iter().map(|r| r.2))),
            ],
        )
        .unwrap();
        let column = |name, physical, logical| {
            Arc::new(
                Type::primitive_type_builder(name, physical)
                    .with_logical_type(Some(logical))
                    .build()
                    .unwrap(),
            )
        };
        let parquet = Type::group_type_builder("schema")
            .with_fields(vec![
                column(
                    "geometry",
                    PhysicalType::BYTE_ARRAY,
                    LogicalType::geometry(Some(crs.to_string())),
                ),
                column("crop", PhysicalType::BYTE_ARRAY, LogicalType::String),
            ])
            .build()
            .unwrap();
        let file = std::fs::File::create(path).unwrap();
        let mut writer = tylertoo_io::SerializedFileWriter::new(
            file,
            Arc::new(parquet),
            Arc::new(WriterProperties::default()),
        )
        .unwrap();
        let mut columns = ArrowRowGroupWriterFactory::new(&writer, Arc::clone(&schema))
            .create_column_writers(0)
            .unwrap();
        for ((array, field), column) in batch
            .columns()
            .iter()
            .zip(schema.fields())
            .zip(&mut columns)
        {
            for leaf in compute_leaves(field, array).unwrap() {
                column.write(&leaf).unwrap();
            }
        }
        let mut row_group = writer.next_row_group().unwrap();
        for column in columns {
            column
                .close()
                .unwrap()
                .append_to_row_group(&mut row_group)
                .unwrap();
        }
        row_group.close().unwrap();
        for (key, value) in metadata {
            writer.append_key_value_metadata(tylertoo_io::KeyValue::new(
                key.to_string(),
                value.clone(),
            ));
        }
        writer.close().unwrap();
    }

    /// A staged layer reads like its input: a CRS only the native type
    /// declares survives staging, whether the input has no `geo` key or one
    /// without that column, and the input's schema descriptor, which a
    /// property selection makes stale, is not copied.
    #[test]
    fn staged_layers_keep_a_native_crs_and_drop_the_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("native-3857.parquet");
        // EPSG:3857 metres for (10E, 10N) and (20W, 5N).
        let rows = [
            (1_113_194.9, 1_118_890.0, "soy"),
            (-2_226_389.8, 557_305.3, "corn"),
        ];
        let no_column = r#"{"version":"2.0.0","columns":{}}"#.to_string();
        for geo in [None, Some(no_column)] {
            let geo: Vec<(&str, String)> = geo.into_iter().map(|g| ("geo", g)).collect();
            // Written twice: the descriptor describes the file as first written.
            write_native_input(&input, "EPSG:3857", &rows, &geo);
            let descriptor = TilesInput::Path(input.clone())
                .source()
                .unwrap()
                .resolved_schema()
                .unwrap()
                .footer_json()
                .unwrap();
            let mut metadata = geo.clone();
            metadata.push((crate::resolved_schema::RESOLVED_SCHEMA_KEY, descriptor));
            write_native_input(&input, "EPSG:3857", &rows, &metadata);

            let out = dir.path().join("o.pmtiles");
            let mut options = options("crop");
            options.convert.properties.exclude_all = true;
            let outcome =
                convert_to_pmtiles(&TilesInput::Path(input.clone()), &out, &options).unwrap();
            let TilesOutcome::Layered(report) = outcome else {
                panic!("expected a layered outcome, got {outcome:?}");
            };
            assert!(report.empty_layers.is_empty(), "{geo:?}: {report:?}");
            for layer in &report.layers {
                assert!(
                    !layer.report.convert.crs_assumed_wgs84,
                    "{geo:?}, {}: CRS assumed, not read",
                    layer.layer
                );
            }
            let at_z3 = layers_at(&out, 3);
            assert_eq!(at_z3.keys().collect::<Vec<_>>(), ["corn", "soy"]);
            // In metres read as degrees, both points would be off the map.
            assert!(
                at_z3.values().all(|features| features.len() == 1),
                "{geo:?}: {at_z3:?}"
            );
        }
    }

    /// Staged files carry the first part's footer, so a multipart input's
    /// version verdict and schema warnings are taken from every part.
    #[test]
    fn a_multipart_input_keeps_every_part_s_version() {
        use crate::diagnostics::{CrsWarning, Diagnostic};
        use crate::resolved_schema::GeoVersion;

        let dir = tempfile::tempdir().unwrap();
        let parts = dir.path().join("parts");
        std::fs::create_dir(&parts).unwrap();
        for (file, version, crop) in [
            ("a.parquet", "1.1.0", "soy"),
            ("b.parquet", "3.0.0", "corn"),
        ] {
            let geo = format!(
                r#"{{"version":"{version}","primary_column":"geometry","columns":{{"geometry":{{"encoding":"WKB","geometry_types":[]}}}}}}"#
            );
            write_native_input(
                &parts.join(file),
                "OGC:CRS84",
                &[(10.0, 10.0, crop)],
                &[("geo", geo)],
            );
        }
        let outcome = convert_to_pmtiles(
            &TilesInput::Path(parts),
            &dir.path().join("o.pmtiles"),
            &options("crop"),
        )
        .unwrap();
        let TilesOutcome::Layered(report) = outcome else {
            panic!("expected a layered outcome, got {outcome:?}");
        };
        let mixed = GeoVersion::Mixed(vec![Some("1.1.0".into()), Some("3.0.0".into())]);
        for layer in &report.layers {
            assert_eq!(layer.report.convert.geo_version, mixed, "{}", layer.layer);
        }
        assert!(
            report.diagnostics.iter().any(|d| matches!(
                d,
                Diagnostic::CrsMetadata { reason: CrsWarning::UnknownVersion(v), .. } if v == "3.0.0"
            )),
            "{:?}",
            report.diagnostics
        );
    }

    #[test]
    fn layer_names_trim_and_bucket_blanks() {
        assert!(
            routable(&DataType::Null),
            "an all-null column routes to _unknown"
        );
        assert_eq!(layer_name(None), UNKNOWN_LAYER);
        assert_eq!(layer_name(Some("")), UNKNOWN_LAYER);
        assert_eq!(layer_name(Some(" \t")), UNKNOWN_LAYER);
        assert_eq!(layer_name(Some(" soy ")), "soy");
    }
}
