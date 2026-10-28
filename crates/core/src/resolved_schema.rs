//! The single owner of input schema interpretation and its persisted overview descriptor.
//!
//! Only this module constructs `ResolvedSchema`. The serialized footer is decoded into
//! private wire data and checked against the physical schema before it becomes a descriptor.
use crate::covering::{CoveringSpec, RowGroupBounds};
use crate::diagnostics::{CoveringWarning, CrsWarning, Diagnostic, Diagnostics};
use crate::overview::level::Crs;
use crate::quality::{is_wgs84_projjson, CrsInfo};
use crate::{Error, Result};
use arrow_schema::{DataType, Schema, SchemaRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use tylertoo_io::LogicalType;
use tylertoo_io::ParquetMetaData;

/// Canonical name for newly created geometry columns and the legacy name fallback.
pub const DEFAULT_GEOMETRY_COLUMN: &str = "geometry";
/// Private-format footer key for resolved schema facts. Older files resolve on open.
pub const RESOLVED_SCHEMA_KEY: &str = "tylertoo:resolved_schema";
/// A spatially cropped shard has no whole-source mass invariant across levels.
pub(crate) const SOURCE_SUBSET_KEY: &str = "tylertoo:source_subset";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SchemaFacts {
    descriptor_version: u8,
    source_is_subset: bool,
    fields: Vec<String>,
    geometry_index: Option<usize>,
    geometry_name: Option<String>,
    primary_column: Option<String>,
    geometry_columns: Vec<String>,
    crs: CrsInfo,
    version: Option<String>,
    properties: Vec<usize>,
    renames: BTreeMap<String, String>,
    covering: Option<CoveringSpec>,
    geo_json: Option<String>,
}

/// Resolved, immutable interpretation of one input part.
///
/// Fields and the wire type are private; callers cannot fabricate a descriptor,
/// including through serde deserialization.
///
/// ```compile_fail
/// use tylertoo_core::resolved_schema::ResolvedSchema;
/// fn replace_facts(schema: &mut ResolvedSchema) { schema.facts = unreachable!(); }
/// ```
///
/// ```compile_fail
/// use tylertoo_core::resolved_schema::ResolvedSchema;
/// let _: ResolvedSchema = serde_json::from_str("{}").unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct ResolvedSchema {
    facts: SchemaFacts,
    schema: SchemaRef,
    bounds: Vec<Option<RowGroupBounds>>,
    diagnostics: Vec<Diagnostic>,
    auto_zoom_covering: std::result::Result<(usize, [String; 4]), CoveringWarning>,
}

impl ResolvedSchema {
    /// Resolve an Arrow schema (for generated data without a Parquet footer).
    ///
    /// # Errors
    /// Returns an error for malformed GeoParquet metadata.
    pub fn resolve(schema: &Schema) -> Result<Self> {
        Self::resolve_with(schema, None)
    }

    /// Carry resolved footer facts through an order-preserving rename or
    /// property projection. In particular, native Parquet CRS declarations
    /// must not be re-inferred from the transformed Arrow schema alone.
    pub(crate) fn transformed(&self, schema: SchemaRef, renames: &[(String, String)]) -> Self {
        let mut result = self.clone();
        let name = |old: &str| {
            renames
                .iter()
                .find(|(from, _)| from == old)
                .map_or_else(|| old.to_owned(), |(_, to)| to.clone())
        };
        result.facts.fields = schema.fields().iter().map(|f| f.name().clone()).collect();
        result.facts.geometry_name = self.geometry_name().map(name);
        result.facts.geometry_index = result.geometry_name().and_then(|n| schema.index_of(n).ok());
        result.facts.primary_column = self.primary_column().map(name);
        result.facts.geometry_columns = self
            .geometry_columns()
            .iter()
            .map(|n| name(n))
            .filter(|n| schema.index_of(n).is_ok())
            .collect();
        result.facts.properties = self
            .properties()
            .iter()
            .filter_map(|&i| schema.index_of(&name(self.schema.field(i).name())).ok())
            .collect();
        // The new overview records this conversion's renames, as the writer
        // did before descriptors were threaded through its stages.
        result.facts.renames = renames
            .iter()
            .map(|(from, to)| (to.clone(), from.clone()))
            .collect();
        result.schema = schema;
        result
    }

    fn resolve_with(schema: &Schema, metadata: Option<&ParquetMetaData>) -> Result<Self> {
        let diagnostics = &Diagnostics::silent();
        let geo_json = schema
            .metadata()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("geo"))
            .map(|(_, v)| v.clone());
        let geo = geo_json
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(|e| {
                Error::GeoParquetRead(format!("Failed to parse geo metadata JSON: {e}"))
            })?;
        if let Some(geo) = &geo {
            warn_geo_version(geo, diagnostics);
        }
        let fields: Vec<String> = schema.fields().iter().map(|f| f.name().clone()).collect();
        let primary_column = geo.as_ref().and_then(primary_name).map(str::to_owned);
        let geometry_columns = geometry_columns(schema, geo.as_ref(), metadata);
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        let selected = geometry_choice(&refs, primary_column.as_deref(), &geometry_columns);
        let geometry_index = selected.and_then(|n| fields.iter().position(|f| f == n));
        let geometry_name = selected.map(str::to_owned);
        let native = metadata.and_then(|md| find_native_geometry_leaf_column(md, selected?));
        let crs = resolve_crs(
            geo.as_ref(),
            selected.or(primary_column.as_deref()),
            native.as_ref().map(|(_, lt)| lt),
            diagnostics,
        );
        let version = geo
            .as_ref()
            .and_then(|g| g.get("version"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| native.as_ref().map(|_| "2.0.0".to_owned()));
        let covering = geo_json
            .as_deref()
            .map(|g| parse_covering_metadata(g, selected))
            .transpose()?
            .flatten();
        let properties =
            property_indices(schema, geometry_index, &geometry_columns, covering.as_ref());
        let mut result = Self {
            facts: SchemaFacts {
                descriptor_version: 1,
                source_is_subset: schema
                    .metadata()
                    .get(SOURCE_SUBSET_KEY)
                    .is_some_and(|v| v == "true"),
                fields,
                geometry_index,
                geometry_name,
                primary_column,
                geometry_columns,
                crs,
                version,
                properties,
                renames: schema_renames(schema),
                covering,
                geo_json,
            },
            schema: Arc::new(schema.clone()),
            bounds: Vec::new(),
            diagnostics: diagnostics.snapshot(),
            auto_zoom_covering: Err(CoveringWarning::NoFooterStatistics),
        };
        result.auto_zoom_covering = result.validate_auto_zoom_covering(metadata);
        if let Some(md) = metadata {
            result.bounds = result.read_bounds(md);
        }
        Ok(result)
    }

    /// Resolve one Parquet part, using a persisted descriptor when present.
    ///
    /// # Errors
    /// Returns an error for a malformed descriptor, schema or geo metadata.
    pub fn from_metadata(metadata: &ParquetMetaData) -> Result<Self> {
        let reader = tylertoo_io::reader_metadata(Arc::new(metadata.clone()))
            .map_err(|e| Error::GeoParquetRead(e.to_string()))?;
        Self::from_parquet(reader.schema(), metadata)
    }

    /// Resolve against an already decoded schema without constructing a reader.
    ///
    /// # Errors
    /// Returns an error for malformed or inconsistent footer facts.
    pub(crate) fn from_parquet(schema: &Schema, metadata: &ParquetMetaData) -> Result<Self> {
        let diagnostics = &Diagnostics::silent();
        let persisted = metadata
            .file_metadata()
            .key_value_metadata()
            .and_then(|kv| kv.iter().find(|kv| kv.key == RESOLVED_SCHEMA_KEY))
            .and_then(|kv| kv.value.as_deref());
        let Some(json) = persisted else {
            return Self::resolve_with(schema, Some(metadata));
        };
        let facts: SchemaFacts = serde_json::from_str(json)
            .map_err(|e| Error::GeoParquetRead(format!("invalid resolved schema: {e}")))?;
        let actual: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
        let geo = schema
            .metadata()
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("geo"))
            .map(|(_, v)| v);
        if facts.descriptor_version != 1
            || facts.geometry_index.is_some_and(|i| i >= actual.len())
            || facts.source_is_subset
                != schema
                    .metadata()
                    .get(SOURCE_SUBSET_KEY)
                    .is_some_and(|v| v == "true")
            || facts.fields.iter().map(String::as_str).collect::<Vec<_>>() != actual
            || facts.geo_json.as_ref() != geo
            || facts.geometry_index.and_then(|i| actual.get(i).copied())
                != facts.geometry_name.as_deref()
            || facts
                .primary_column
                .as_ref()
                .is_some_and(|n| Some(n) != facts.geometry_name.as_ref())
            || facts
                .properties
                .iter()
                .any(|i| *i >= actual.len() || Some(*i) == facts.geometry_index)
        {
            return Err(Error::GeoParquetRead(
                "resolved schema disagrees with the Parquet footer".into(),
            ));
        }
        let declared = facts
            .geo_json
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(|e| Error::GeoParquetRead(format!("invalid geo metadata: {e}")))?;
        if let Some(geo) = &declared {
            warn_geo_version(geo, diagnostics);
        }
        let native = facts
            .geometry_name
            .as_deref()
            .and_then(|n| find_native_geometry_leaf_column(metadata, n));
        let crs = resolve_crs(
            declared.as_ref(),
            facts
                .geometry_name
                .as_deref()
                .or(facts.primary_column.as_deref()),
            native.as_ref().map(|(_, lt)| lt),
            diagnostics,
        );
        let covering = facts
            .geo_json
            .as_deref()
            .map(|j| parse_covering_metadata(j, facts.geometry_name.as_deref()))
            .transpose()?
            .flatten();
        let declared_primary = declared.as_ref().and_then(primary_name);
        let version = declared
            .as_ref()
            .and_then(|g| g.get("version"))
            .and_then(Value::as_str)
            .or_else(|| native.as_ref().map(|_| "2.0.0"));
        let geometry_columns = geometry_columns(schema, declared.as_ref(), Some(metadata));
        let selected = geometry_choice(&actual, declared_primary, &geometry_columns);
        let properties = property_indices(
            schema,
            facts.geometry_index,
            &geometry_columns,
            covering.as_ref(),
        );
        if crs != facts.crs
            || covering != facts.covering
            || facts.primary_column.as_deref() != declared_primary
            || version != facts.version.as_deref()
            || facts.renames != schema_renames(schema)
            || selected != facts.geometry_name.as_deref()
            || facts.geometry_index != selected.and_then(|n| actual.iter().position(|f| *f == n))
            || geometry_columns != facts.geometry_columns
            || properties != facts.properties
        {
            return Err(Error::GeoParquetRead(
                "resolved schema disagrees with the GeoParquet declarations".into(),
            ));
        }
        let mut result = Self {
            facts,
            schema: Arc::new(schema.clone()),
            bounds: Vec::new(),
            diagnostics: diagnostics.snapshot(),
            auto_zoom_covering: Err(CoveringWarning::NoFooterStatistics),
        };
        result.auto_zoom_covering = result.validate_auto_zoom_covering(Some(metadata));
        result.bounds = result.read_bounds(metadata);
        Ok(result)
    }

    /// Replay immutable schema warnings once into the current operation.
    pub(crate) fn record_diagnostics(&self, sink: &Diagnostics) {
        for event in &self.diagnostics {
            sink.once(event.clone());
        }
    }

    /// Serialize the output descriptor, including rename provenance.
    ///
    /// # Errors
    /// Returns an error if serialization fails.
    pub(crate) fn footer_json(&self) -> Result<String> {
        serde_json::to_string(&self.facts).map_err(|e| Error::GeoParquetRead(e.to_string()))
    }
    /// WKB geometry fields whose byte values need structural validation before encoding.
    pub(crate) fn wkb_columns(&self) -> Vec<(usize, String)> {
        self.schema
            .fields()
            .iter()
            .enumerate()
            .filter(|(_, f)| {
                f.metadata()
                    .get("ARROW:extension:name")
                    .is_some_and(|n| n == "geoarrow.wkb")
            })
            .map(|(i, f)| (i, f.name().clone()))
            .collect()
    }

    /// Whether levels were independently cropped to a shard's spatial domain.
    pub(crate) fn source_is_subset(&self) -> bool {
        self.facts.source_is_subset
    }

    /// Primary geometry column index in the full schema.
    pub fn geometry_index(&self) -> Option<usize> {
        self.facts.geometry_index
    }
    /// Primary geometry column name, also valid in projected batches.
    pub fn geometry_name(&self) -> Option<&str> {
        self.facts.geometry_name.as_deref()
    }
    /// Explicit primary-column declaration, if any.
    pub fn primary_column(&self) -> Option<&str> {
        self.facts.primary_column.as_deref()
    }
    /// Every geometry field, in schema order.
    pub fn geometry_columns(&self) -> &[String] {
        &self.facts.geometry_columns
    }
    /// CRS declaration and whether WGS84 was assumed.
    pub fn crs_info(&self) -> &CrsInfo {
        &self.facts.crs
    }
    /// Supported coordinate system, or `None` for unknown/unsupported declarations.
    pub fn crs(&self) -> Option<Crs> {
        match crate::quality::classify_crs_info(self.crs_info()) {
            crate::quality::CrsKind::Wgs84 => Some(Crs::Epsg4326),
            crate::quality::CrsKind::WebMercator => Some(Crs::Epsg3857),
            crate::quality::CrsKind::Unknown => None,
        }
    }
    /// GeoParquet version, including native 2.0 without a geo key.
    pub fn version(&self) -> Option<&str> {
        self.facts.version.as_deref()
    }
    /// Schema indices of data properties, excluding all geometry and covering fields.
    pub fn properties(&self) -> &[usize] {
        &self.facts.properties
    }
    /// Output name to source name provenance.
    pub fn renames(&self) -> &BTreeMap<String, String> {
        &self.facts.renames
    }
    /// Covering of the primary geometry, never another column's.
    pub fn covering(&self) -> Option<&CoveringSpec> {
        self.facts.covering.as_ref()
    }
    /// Conservative per-row-group bounds from covering or native statistics.
    pub fn bounds(&self) -> &[Option<RowGroupBounds>] {
        &self.bounds
    }
    /// Full Arrow schema used to resolve this descriptor.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// A bit-exact f64 bbox covering for the geometry column at `geom_idx`
    /// (#693; also the export scan's bbox source since #730): the root (schema)
    /// index of the covering struct column and its four child-field names, in
    /// `xmin, ymin, xmax, ymax` order as spelled in the file's own schema.
    ///
    /// `None` — and the caller then reads the geometry itself — unless ALL of:
    ///
    /// - the schema carries "geo" metadata whose covering belongs to the geometry
    ///   column the conversion reads ([`parse_covering_metadata`] enforces the
    ///   per-column ownership, #519);
    /// - every covering path is `[struct, field]` on one shared top-level struct
    ///   (the only shape the GeoParquet 1.1 spec describes);
    /// - all four fields are `Float64`. The spec allows `FLOAT` too (Overture
    ///   writes f32), but an f32 bound is rounded to the nearest float32, NOT
    ///   bit-identical to the f64 bbox the geometry scalars yield — centers and
    ///   diagonals would shift and could move a marginal zoom. gpio (the
    ///   documented input contract) writes f64, so the fast path covers it; an
    ///   f32 covering simply keeps the exact geometry path;
    /// - all four paths select distinct leaves.
    ///
    /// Name matching is case-insensitive, like
    /// [`crate::covering::find_bbox_column_indices`] on the pruning side.
    pub(crate) fn f64_covering(&self) -> Option<(usize, [String; 4])> {
        self.f64_covering_shape().ok()
    }

    /// Auto-zoom additionally requires finite footer extrema within the CRS's
    /// drawable range (including longitude wrap) in every nonempty row group.
    /// Eligibility is cached from the actual footer, including for persisted
    /// descriptors. In-range stale values remain undetectable by this guard.
    pub(crate) fn auto_zoom_covering<'a>(
        parts: impl IntoIterator<Item = &'a Self>,
        sink: &Diagnostics,
    ) -> Option<(usize, [String; 4])> {
        let mut covering = None;
        let mut rejected = false;
        for part in parts {
            match &part.auto_zoom_covering {
                Ok(candidate) if covering.as_ref().is_none_or(|c| c == candidate) => {
                    covering = Some(candidate.clone());
                }
                result => {
                    let reason = result
                        .as_ref()
                        .err()
                        .cloned()
                        .unwrap_or(CoveringWarning::InconsistentPartitions);
                    sink.once(Diagnostic::CoveringRejected { count: 1, reason });
                    rejected = true;
                }
            }
        }
        covering.filter(|_| !rejected)
    }

    fn f64_covering_shape(&self) -> std::result::Result<(usize, [String; 4]), CoveringWarning> {
        let spec = self.covering().ok_or(CoveringWarning::NoCovering)?;
        let schema = self.schema();
        let paths = [
            &spec.xmin_path,
            &spec.ymin_path,
            &spec.xmax_path,
            &spec.ymax_path,
        ];
        if paths.iter().any(|p| p.len() != 2) {
            return Err(CoveringWarning::UnsupportedPath);
        }
        let root = &paths[0][0];
        if paths.iter().any(|p| !p[0].eq_ignore_ascii_case(root)) {
            return Err(CoveringWarning::UnsupportedPath);
        }
        let (idx, field) = schema
            .fields()
            .iter()
            .enumerate()
            .find(|(_, f)| f.name().eq_ignore_ascii_case(root))
            .ok_or(CoveringWarning::MissingColumn)?;
        let DataType::Struct(children) = field.data_type() else {
            return Err(CoveringWarning::UnsupportedPath);
        };
        let mut names: [String; 4] = Default::default();
        for (slot, p) in names.iter_mut().zip(paths) {
            let child = children
                .iter()
                .find(|c| c.name().eq_ignore_ascii_case(&p[1]))
                .ok_or(CoveringWarning::MissingColumn)?;
            if *child.data_type() != DataType::Float64 {
                return Err(CoveringWarning::NotFloat64);
            }
            *slot = child.name().clone();
        }
        if names
            .iter()
            .map(|n| n.to_ascii_lowercase())
            .collect::<HashSet<_>>()
            .len()
            != 4
        {
            return Err(CoveringWarning::DuplicateColumns);
        }
        Ok((idx, names))
    }

    fn validate_auto_zoom_covering(
        &self,
        metadata: Option<&ParquetMetaData>,
    ) -> std::result::Result<(usize, [String; 4]), CoveringWarning> {
        let covering = self.f64_covering_shape()?;
        let spec = self.covering().ok_or(CoveringWarning::NoCovering)?;
        let md = metadata.ok_or(CoveringWarning::NoFooterStatistics)?;
        let indices = crate::covering::find_bbox_column_indices(md, spec)
            .ok_or(CoveringWarning::MissingColumn)?;
        for (rg_idx, rg) in md.row_groups().iter().enumerate() {
            if !self.validate_covering_row_group(rg_idx, rg, indices)? {
                return Err(CoveringWarning::MissingStatistics(rg_idx));
            }
        }
        Ok(covering)
    }

    /// The shared trust decision for sampling and spatial pruning. Pruning
    /// keeps a rejected group for geometry decoding, including when another
    /// group's covering is usable. FLOAT coverings can still prune; sampling
    /// separately requires f64 to preserve its bit-exact geometry oracle.
    /// Returns whether all statistics are finite: ordered infinities (including
    /// the conventional empty envelope) can prune but cannot supply a sample.
    pub(crate) fn validate_covering_row_group(
        &self,
        rg_idx: usize,
        rg: &tylertoo_io::RowGroupMetaData,
        indices: crate::covering::BboxColumnIndices,
    ) -> std::result::Result<bool, CoveringWarning> {
        let crs = self.crs().ok_or(CoveringWarning::UnknownCrs)?;
        let range = crate::overview::convert::crs_coordinate_range(crs);
        if [indices.xmin, indices.ymin, indices.xmax, indices.ymax]
            .into_iter()
            .collect::<HashSet<_>>()
            .len()
            != 4
        {
            return Err(CoveringWarning::DuplicateColumns);
        }
        if rg.num_rows() == 0 {
            return Ok(true);
        }
        let mut finite = true;
        // Check both extrema of all four leaves: inverted/dateline boxes
        // can otherwise hide an out-of-range endpoint in the inner stats.
        for (col, is_x) in [
            (indices.xmin, true),
            (indices.ymin, false),
            (indices.xmax, true),
            (indices.ymax, false),
        ] {
            for min in [true, false] {
                let value = crate::covering::extract_stat_value(rg, col, min)
                    .ok_or(CoveringWarning::MissingStatistics(rg_idx))?;
                if !value.is_finite() {
                    finite = false;
                } else if value.abs() > if is_x { 3.0 * range.max_x } else { range.max_y } {
                    return Err(CoveringWarning::OutOfCrsRange(rg_idx));
                }
            }
        }
        Ok(finite)
    }

    /// Validate output → source rename provenance against the physical fields.
    /// A source name denotes the column the producer reserved in this file;
    /// the original input schema is not part of an overview footer.
    pub(crate) fn validate_renames(&self) -> std::result::Result<(), String> {
        let mut problems = Vec::new();
        for (output, source) in self.renames() {
            if self.schema().column_with_name(output).is_none() {
                problems.push(format!("output column {output:?} is absent"));
            }
            if !self
                .schema()
                .fields()
                .iter()
                .any(|f| f.name().eq_ignore_ascii_case(source))
            {
                problems.push(format!("source column {source:?} is absent"));
            }
            if output.eq_ignore_ascii_case(source) {
                problems.push(format!("{output:?} maps to itself"));
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("; "))
        }
    }

    /// Restore data property names without colliding with published columns.
    pub(crate) fn publication_names(
        &self,
        suppressed: &HashSet<String>,
    ) -> HashMap<String, String> {
        publication_names(self.schema(), self.renames(), suppressed)
    }
    /// Another annotated geometry field selected by the legacy name heuristic.
    pub(crate) fn geometry_name_disagreement(&self) -> Option<&str> {
        geometry_name_disagreement(self.schema(), self.geometry_index()?)
    }

    /// Select groups conservatively, keeping groups without usable statistics.
    pub(crate) fn select_row_groups(&self, boxes: &[[f64; 4]]) -> Vec<usize> {
        let filters: Vec<crate::tile::TileBounds> = boxes
            .iter()
            .map(|b| crate::tile::TileBounds {
                lng_min: b[0],
                lat_min: b[1],
                lng_max: b[2],
                lat_max: b[3],
            })
            .collect();
        self.bounds
            .iter()
            .enumerate()
            .filter(|(_, b)| match b {
                None => true,
                Some(b) => {
                    let tested = if b.xmin > b.xmax {
                        &filters[..filters.len().min(1)]
                    } else {
                        &filters[..]
                    };
                    tested.iter().any(|f| b.intersects(f))
                }
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn read_bounds(&self, md: &ParquetMetaData) -> Vec<Option<RowGroupBounds>> {
        let tier1 = crate::covering::bounds_from_resolved(md, self);
        let native = self
            .geometry_name()
            .and_then(|n| find_native_geometry_leaf_column(md, n))
            .filter(|(_, lt)| {
                self.crs()
                    .is_some_and(|crs| native_geo_crs_matches(lt, crs))
            })
            .map(|(i, _)| crate::covering::geo_statistics_bounds(md, i));
        match native {
            None => tier1,
            Some(native) => tier1
                .into_iter()
                .zip(native)
                .map(|(covering, native)| covering.or(native))
                .collect(),
        }
    }
}

fn geometry_choice<'a>(
    names: &[&'a str],
    primary: Option<&'a str>,
    columns: &'a [String],
) -> Option<&'a str> {
    primary
        .filter(|n| names.contains(n))
        .or_else(|| geometry_column_by_name(names))
        .or_else(|| columns.first().map(String::as_str))
}

fn geometry_columns(
    schema: &Schema,
    geo: Option<&Value>,
    metadata: Option<&ParquetMetaData>,
) -> Vec<String> {
    schema
        .fields()
        .iter()
        .filter(|f| {
            f.metadata()
                .get("ARROW:extension:name")
                .is_some_and(|n| n.starts_with("geoarrow"))
                || geo
                    .and_then(|g| g.get("columns"))
                    .and_then(|c| c.get(f.name()))
                    .is_some()
                || metadata
                    .is_some_and(|md| find_native_geometry_leaf_column(md, f.name()).is_some())
        })
        .map(|f| f.name().clone())
        .collect()
}

fn property_indices(
    schema: &Schema,
    geometry_index: Option<usize>,
    geometry_columns: &[String],
    covering: Option<&CoveringSpec>,
) -> Vec<usize> {
    let geometry = geometry_index.map(|i| schema.field(i).name().as_str());
    schema
        .fields()
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            Some(*i) != geometry_index
                && !geometry_columns.contains(f.name())
                && (!schema
                    .metadata()
                    .contains_key(crate::overview::level::OVERVIEWS_KEY)
                    || !f
                        .name()
                        .eq_ignore_ascii_case(crate::overview::writer::LEVEL_COLUMN))
                && !crate::overview::writer::is_covering_column(
                    f,
                    geometry,
                    covering
                        .and_then(|c| c.xmin_path.first())
                        .map(String::as_str),
                )
        })
        .map(|(i, _)| i)
        .collect()
}

fn resolve_crs(
    geo: Option<&Value>,
    selected: Option<&str>,
    native: Option<&LogicalType>,
    diagnostics: &Diagnostics,
) -> CrsInfo {
    if let Some(col) = geo
        .and_then(|g| g.get("columns"))
        .and_then(|c| c.get(selected?))
    {
        crs_from_column(col, diagnostics)
    } else if let Some(lt) = native {
        let id = match lt {
            LogicalType::Geometry(g) => g.crs.as_deref(),
            LogicalType::Geography(g) => g.crs.as_deref(),
            _ => None,
        };
        match id {
            None => CrsInfo::wgs84(),
            Some(id) => CrsInfo::from_identifier(id),
        }
    } else {
        let reason = match geo {
            None => CrsWarning::NoGeoMetadata,
            Some(g) if !g.get("columns").is_some_and(Value::is_object) => CrsWarning::NoColumns,
            Some(_) => {
                CrsWarning::MissingColumn(selected.unwrap_or(DEFAULT_GEOMETRY_COLUMN).to_owned())
            }
        };
        diagnostics.once(Diagnostic::CrsMetadata { count: 1, reason });
        CrsInfo::wgs84_assumed()
    }
}

fn schema_renames(schema: &Schema) -> BTreeMap<String, String> {
    schema
        .metadata()
        .get(crate::overview::level::OVERVIEWS_KEY)
        .and_then(|j| crate::overview::level::OverviewsMeta::from_json(j).ok())
        .and_then(|m| m.generalization.and_then(|g| g.renamed_columns))
        .unwrap_or_default()
}

fn primary_name(geo: &Value) -> Option<&str> {
    geo.get("primary_column").and_then(Value::as_str)
}

fn geometry_column_by_name<'a>(names: &[&'a str]) -> Option<&'a str> {
    names
        .iter()
        .find(|n| **n == DEFAULT_GEOMETRY_COLUMN)
        .or_else(|| names.iter().find(|n| n.contains("geom")))
        .copied()
}

/// Internal structures for parsing GeoParquet JSON metadata.
#[derive(Debug, Deserialize)]
struct GeoMetadata {
    columns: HashMap<String, ColumnMetadata>,
    #[serde(default)]
    primary_column: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ColumnMetadata {
    #[serde(default)]
    covering: Option<CoveringMetadata>,
}

#[derive(Debug, Deserialize)]
struct CoveringMetadata {
    bbox: Option<BboxCovering>,
}

#[derive(Debug, Deserialize)]
struct BboxCovering {
    xmin: Vec<String>,
    ymin: Vec<String>,
    xmax: Vec<String>,
    ymax: Vec<String>,
}

/// Parse the covering specification from GeoParquet geo metadata JSON, for
/// ONE named geometry column.
///
/// # Arguments
///
/// * `geo_json` - The JSON string from the "geo" key-value metadata
/// * `geom_column` - the column whose covering is wanted: for pruning, the
///   column the rest of the pipeline will actually read
///   (`ResolvedSchema::geometry_name`). `None` means the caller has no
///   column in hand, and then the spec-REQUIRED `primary_column` is the only
///   thing consulted.
///
/// There is deliberately **no** "first column that happens to declare a
/// covering" fallback (#519). A file with a `centroid` covering beside a
/// `geom_shape` the pipeline reads got pruned on the centroid's envelope —
/// silent data loss — and, with two covered columns, the fallback scanned a
/// `HashMap`'s values, so *which* wrong column won varied run to run. A
/// column with no covering of its own simply has no tier-1 bounds; the row
/// group is then read, which is always correct.
///
/// # Returns
///
/// `Ok(Some(CoveringSpec))` if that column declares a valid covering,
/// `Ok(None)` if it does not (or is absent from the metadata),
/// `Err` if the JSON is malformed.
///
/// # Errors
///
/// Returns [`Error::GeoParquetRead`] if `geo_json` is not valid geo metadata
/// JSON.
pub(crate) fn parse_covering_metadata(
    geo_json: &str,
    geom_column: Option<&str>,
) -> Result<Option<CoveringSpec>> {
    let metadata: GeoMetadata = serde_json::from_str(geo_json)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to parse geo metadata JSON: {}", e)))?;

    let name = geom_column.or(metadata.primary_column.as_deref());
    let Some(column) = name.and_then(|n| metadata.columns.get(n)) else {
        return Ok(None);
    };

    let Some(covering) = &column.covering else {
        return Ok(None);
    };

    let Some(bbox) = &covering.bbox else {
        return Ok(None);
    };

    Ok(Some(CoveringSpec::new(
        bbox.xmin.clone(),
        bbox.ymin.clone(),
        bbox.xmax.clone(),
        bbox.ymax.clone(),
    )))
}

/// What a `geo` metadata `version` field says about how safely this reader
/// can interpret the rest of the metadata (#451).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeoVersionDisposition {
    /// A GeoParquet spec this reader knows: major 1 (1.0/1.1) or major 2
    /// (parquet-native geometry types, which the Parquet `LogicalType`
    /// readers handle — see #518).
    Known,
    /// The spec-required `version` field is absent.
    Missing,
    /// A major this reader does not know (a future 3.x, a pre-1.0 beta), or
    /// a string that is not a version at all. The metadata is still read
    /// best-effort, but fields may be misread.
    Unknown,
}

/// Classify a `geo` metadata `version` string (#451).
///
/// The major is everything before the first `.` or `-`, so pre-release
/// suffixes ("1.0.0-beta.1") classify by their major like releases do.
pub(crate) fn geo_version_disposition(version: Option<&str>) -> GeoVersionDisposition {
    let Some(version) = version else {
        return GeoVersionDisposition::Missing;
    };
    let major = version.trim().split(['.', '-']).next().unwrap_or("");
    match major.parse::<u32>() {
        Ok(1 | 2) => GeoVersionDisposition::Known,
        _ => GeoVersionDisposition::Unknown,
    }
}

/// Warn (once per run) when a `geo` metadata `version` is missing or
/// names a spec this reader does not know (#451). A warning, never an error:
/// the fields read from the metadata (`primary_column`, `crs`, `bbox`,
/// `covering`) are read best-effort and a future spec most likely still
/// carries them, so rejecting the file outright would refuse inputs that
/// parse fine — matching the issue's "hard-warn on unknown majors" ask.
fn warn_geo_version(geo_json: &Value, diagnostics: &crate::diagnostics::Diagnostics) {
    let version = geo_json.get("version").and_then(Value::as_str);
    match geo_version_disposition(version) {
        GeoVersionDisposition::Known => {}
        GeoVersionDisposition::Missing => {
            diagnostics.once(crate::diagnostics::Diagnostic::CrsMetadata {
                count: 1,
                reason: crate::diagnostics::CrsWarning::MissingVersion,
            });
        }
        GeoVersionDisposition::Unknown => {
            // The version string is untrusted input headed for a log line;
            // {:?} escapes it and the cap keeps a pathological value short.
            let version = version.unwrap_or_default();
            let shown: String = version.chars().take(64).collect();
            diagnostics.once(crate::diagnostics::Diagnostic::CrsMetadata {
                count: 1,
                reason: crate::diagnostics::CrsWarning::UnknownVersion(shown),
            });
        }
    }
}

/// Extract CRS information from already-parsed parquet key-value metadata.
///
/// The metadata-only core of [`extract_crs`], shared with input paths that
/// have a parsed footer in hand already (e.g. remote inputs, #210, where the
/// footer was range-fetched once and re-opening the file would cost another
/// round trip).
///
/// # Errors
///
/// Returns [`Error::GeoParquetRead`] if the geo metadata JSON is malformed.
pub(crate) fn crs_info_from_kv_metadata(
    kv_metadata: Option<&Vec<tylertoo_io::KeyValue>>,
    diagnostics: &Diagnostics,
) -> Result<CrsInfo> {
    // Look for the "geo" key in key-value metadata
    let Some(kv_metadata) = kv_metadata else {
        // No metadata at all - assume WGS84 with warning
        diagnostics.once(Diagnostic::CrsMetadata {
            count: 1,
            reason: CrsWarning::NoKeyValueMetadata,
        });
        return Ok(CrsInfo::wgs84_assumed());
    };

    let geo_value = kv_metadata
        .iter()
        .find(|kv| kv.key.to_lowercase() == "geo")
        .and_then(|kv| kv.value.as_ref());

    let Some(geo_json_str) = geo_value else {
        // No geo metadata - assume WGS84 with warning
        diagnostics.once(Diagnostic::CrsMetadata {
            count: 1,
            reason: CrsWarning::NoGeoMetadata,
        });
        return Ok(CrsInfo::wgs84_assumed());
    };

    // Parse the geo metadata JSON
    let geo_json: Value = serde_json::from_str(geo_json_str)
        .map_err(|e| Error::GeoParquetRead(format!("Failed to parse geo metadata JSON: {}", e)))?;

    // #451: flag a `geo.version` from a spec this reader does not know
    // before interpreting any of its fields.
    warn_geo_version(&geo_json, diagnostics);

    let primary_column = primary_name(&geo_json).unwrap_or(DEFAULT_GEOMETRY_COLUMN);

    // Get the columns object
    let Some(columns) = geo_json.get("columns").and_then(Value::as_object) else {
        diagnostics.once(Diagnostic::CrsMetadata {
            count: 1,
            reason: CrsWarning::NoColumns,
        });
        return Ok(CrsInfo::wgs84_assumed());
    };

    // Get the primary column's metadata
    let Some(column_meta) = columns.get(primary_column) else {
        diagnostics.once(Diagnostic::CrsMetadata {
            count: 1,
            reason: CrsWarning::MissingColumn(primary_column.to_owned()),
        });
        return Ok(CrsInfo::wgs84_assumed());
    };

    Ok(crs_from_column(column_meta, diagnostics))
}

fn crs_from_column(column_meta: &Value, diagnostics: &Diagnostics) -> CrsInfo {
    // Extract CRS - can be a string identifier or PROJJSON object
    let crs = column_meta.get("crs");

    match crs {
        None => {
            // No CRS specified - GeoParquet spec says this means WGS84
            CrsInfo::wgs84()
        }
        Some(Value::Null) => {
            // Explicit null means "no CRS assigned" (spec-distinct from an
            // omitted key = OGC:CRS84), but real-world writers (Spark/Sedona,
            // e.g. the FTW predictions collection) emit it on lon/lat data.
            // Assume CRS84 unless the declared bbox proves the coordinates
            // can't be degrees.
            let bbox_plausible_degrees = column_meta
                .get("bbox")
                .and_then(Value::as_array)
                .and_then(|b| {
                    let v: Vec<f64> = b.iter().filter_map(Value::as_f64).collect();
                    // GeoParquet orders a 2D bbox [xmin, ymin, xmax, ymax]
                    // and a 3D one [xmin, ymin, zmin, xmax, ymax, zmax], so
                    // the x/y maxima move. Reading a 6-element bbox with the
                    // 2D offsets compares a *z* minimum against +-180 and an
                    // x maximum against +-90, which rejects any lon/lat file
                    // east of 90E -- e.g. a Sedona-written Tokyo extract
                    // with "crs": null and bbox
                    // [139.6, 35.6, 0.0, 139.8, 35.8, 50.0].
                    let idx: Option<[usize; 4]> = match v.len() {
                        4 => Some([0, 1, 2, 3]),
                        6 => Some([0, 1, 3, 4]),
                        // Any other length is not a bbox we understand;
                        // treat it as absent rather than guessing offsets.
                        _ => None,
                    };
                    idx.map(|[xmin, ymin, xmax, ymax]| {
                        v[xmin] >= -180.0 && v[xmax] <= 180.0 && v[ymin] >= -90.0 && v[ymax] <= 90.0
                    })
                })
                // No (or malformed) bbox: nothing to check against — match
                // the missing-geo-metadata precedent and assume.
                .unwrap_or(true);
            if bbox_plausible_degrees {
                diagnostics.once(Diagnostic::CrsMetadata {
                    count: 1,
                    reason: CrsWarning::NullCrsDegrees,
                });
                CrsInfo::wgs84_assumed()
            } else {
                diagnostics.once(Diagnostic::CrsMetadata {
                    count: 1,
                    reason: CrsWarning::NullCrsUnknown,
                });
                CrsInfo::unknown()
            }
        }
        Some(Value::String(crs_str)) => {
            // Simple CRS identifier (e.g., "EPSG:4326")
            CrsInfo::from_identifier(crs_str)
        }
        Some(crs_obj @ Value::Object(_)) => {
            // PROJJSON object
            let is_wgs84 = is_wgs84_projjson(crs_obj);
            let name = crs_obj.get("name").and_then(Value::as_str);
            let id_str = crs_obj.get("id").map(|id| {
                let authority = id.get("authority").and_then(Value::as_str).unwrap_or("");
                let code = id
                    .get("code")
                    .map(|c| {
                        c.as_str()
                            .map(|s| s.to_string())
                            .or_else(|| c.as_i64().map(|n| n.to_string()))
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                format!("{}:{}", authority, code)
            });

            CrsInfo {
                identifier: id_str.or_else(|| name.map(|n| n.to_string())),
                is_wgs84,
                name: name.map(|s| s.to_string()),
                assumed_wgs84: false,
            }
        }
        Some(other) => {
            diagnostics.once(Diagnostic::CrsMetadata {
                count: 1,
                reason: CrsWarning::UnexpectedFormat(format!("{other:?}")),
            });
            CrsInfo::unknown()
        }
    }
}

pub(crate) fn publication_names(
    schema: &Schema,
    renames: &BTreeMap<String, String>,
    suppressed: &HashSet<String>,
) -> HashMap<String, String> {
    // Names already spoken for by a column that will publish under its own
    // schema name. `level` is excluded because the export drops it, which
    // is precisely what frees the name for restoration; a suppressed
    // counter is excluded for the same reason.
    let occupied: HashSet<String> = schema
        .fields()
        .iter()
        .filter(|f| {
            !f.name()
                .eq_ignore_ascii_case(crate::overview::writer::LEVEL_COLUMN)
                && !suppressed.contains(f.name())
        })
        .map(|f| f.name().to_ascii_lowercase())
        .collect();

    // Fold rather than filter: each accepted restoration *takes* its source
    // name, so the next one sees it. Two columns can be renamed away from
    // one source name — `resolve_reserved_column_collisions` iterates
    // fields, not distinct names, so an input carrying two columns both
    // called `level` yields `{"level_": "level", "level__": "level"}` —
    // and restoring both would put the same key on a feature twice, which
    // is worse than the renamed name and worse than what `main` produces
    // for that input. `HashSet::insert` returning false covers the
    // already-a-column case and the already-restored case in one check.
    // `renames` is a `BTreeMap`, so which of the two wins is deterministic.
    let mut occupied = occupied;
    let mut restored: HashMap<String, String> = HashMap::new();
    for (renamed, source) in renames {
        // Only touch columns this file actually has.
        if schema.column_with_name(renamed).is_none() {
            continue;
        }
        if !occupied.insert(source.to_ascii_lowercase()) {
            continue;
        }
        restored.insert(renamed.clone(), source.clone());
    }

    restored
}

pub(crate) fn geometry_name_disagreement(schema: &Schema, geom_idx: usize) -> Option<&str> {
    let names: Vec<&str> = schema.fields().iter().map(|f| f.name().as_str()).collect();
    let by_name = geometry_column_by_name(&names)?;
    let annotated = schema.field_with_name(by_name).is_ok_and(|f| {
        f.metadata()
            .get("ARROW:extension:name")
            .is_some_and(|n| n.starts_with("geoarrow"))
    });
    (Some(by_name) != names.get(geom_idx).copied() && annotated).then_some(by_name)
}

#[cfg(test)]
mod tests;

/// Parquet leaf-column index of `geom_column`'s GeoParquet 2.0 geometry
/// annotation (`LogicalType::Geometry` / `Geography`) — no "geo" JSON
/// required, which is exactly the shape of a pure GeoParquet 2.0 file.
///
/// The leaf MUST belong to `geom_column` (matched on the schema path's root
/// component): a file with several annotated columns — say a `centroid`
/// alongside the real `geom_shape` — otherwise prunes on whichever envelope
/// happens to come first in schema order while the pipeline reads a
/// different column, silently dropping every matching feature (#518). No
/// match means no tier-2 pruning, never a guess.
pub(crate) fn find_native_geometry_leaf_column(
    metadata: &ParquetMetaData,
    geom_column: &str,
) -> Option<(usize, LogicalType)> {
    let schema = metadata.file_metadata().schema_descr();

    (0..schema.num_columns()).find_map(|col_idx| {
        let col = schema.column(col_idx);
        let lt = col.logical_type_ref()?;
        if !matches!(lt, LogicalType::Geometry(_) | LogicalType::Geography(_)) {
            return None;
        }
        // `parts()[0]` is the root field: the leaf itself for a top-level
        // primitive column, the enclosing field for a nested one.
        if col.path().parts().first().map(String::as_str) != Some(geom_column) {
            return None;
        }
        Some((col_idx, lt.clone()))
    })
}

/// CRS classification for `LogicalType::Geometry(crs)` / `Geography(crs)`
/// annotations, mapped onto the two CRSs [`crate::overview::level::Crs`]
/// supports. `None` means "unresolvable" and
/// [`native_geo_crs_matches`] turns that into "never guess" — tier-2
/// pruning is skipped entirely.
///
/// Delegates to [`crate::quality::classify_crs_identifier`], the single
/// classifier shared with the legacy "geo" JSON path. It used to be a
/// verbatim copy of an older substring match that tested `contains("WGS 84")`
/// BEFORE `contains("3857")` — and EPSG:3857's PROJJSON (what arrow-rs
/// inlines here) is named "WGS 84 / Pseudo-Mercator", so a Web Mercator file
/// classified as lon/lat and its meter envelopes were filtered against a
/// degree bbox: every row group pruned, a successful EMPTY archive (#518).
fn crs_identifier_kind(id: &str) -> Option<Crs> {
    match crate::quality::classify_crs_identifier(id) {
        crate::quality::CrsKind::Wgs84 => Some(Crs::Epsg4326),
        crate::quality::CrsKind::WebMercator => Some(Crs::Epsg3857),
        crate::quality::CrsKind::Unknown => None,
    }
}

/// Whether a native geometry column's declared CRS is consistent with
/// `session_crs` — the CRS the rest of the conversion has already committed
/// to via [`crate::overview::convert::detect_crs_from_kv`]. Tier-2 pruning
/// is only safe when this holds; the caller must fall through to tier 3
/// (read everything) rather than guess (#497).
///
/// DIVERGENCE FROM TIPPECANOE (N/A — no tippecanoe equivalent; this is a
/// GeoParquet 2.0 concern): a `Geography` column is excluded
/// unconditionally. Its edges interpolate along the sphere (non-planar), so
/// a corner-only AABB test does not necessarily bound the true shape the
/// way it does for a planar `Geometry` column — a great-circle arc can bulge
/// outside the box its endpoints describe. Conservative: never prune it.
pub(crate) fn native_geo_crs_matches(logical_type: &LogicalType, session_crs: Crs) -> bool {
    let LogicalType::Geometry(g) = logical_type else {
        return false;
    };
    let resolved = match &g.crs {
        None => Some(Crs::Epsg4326), // unset ⇒ OGC:CRS84 per the Parquet spec
        Some(s) => crs_identifier_kind(s),
    };
    resolved == Some(session_crs)
}
