//! Syntax shared by CLI flags and Python keywords. Ordinary values are
//! deserialized into the engine options; there is no second option model.

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

use super::SizeCap;
use super::{class_ranking, collapse_mode, entry_zoom, representation, ConfigError};
use crate::overview::convert::{ConvertError, ConvertOptions, GeneralizationOverrides, LevelPlan};
use crate::overview::export::ExportOptions;

/// Transfer a keyword value without erasing non-finite numbers as null.
///
/// # Errors
/// Returns a diagnostic naming the keyword for unsupported or non-finite values.
pub fn deserialize_value<'de>(
    keyword: &str,
    deserializer: impl serde::Deserializer<'de>,
) -> Result<Value, ConfigError> {
    super::values::deserialize(deserializer)
        .map_err(|detail| error("keywords", format!("{keyword}: {detail}")))
}

/// Structured class-rank syntax, shared with the native Python deserializer.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct ClassRankSyntax {
    class_rank: Option<String>,
    class_rank_column: Option<String>,
    class_ranks: Option<std::collections::BTreeMap<String, f64>>,
    class_rank_unknown: Option<f64>,
}

impl ClassRankSyntax {
    /// Check ranks before a JSON-backed surface can erase NaN or infinity.
    ///
    /// # Errors
    /// Returns the core rank diagnostic for a non-finite priority.
    fn validate(&self) -> Result<(), ConfigError> {
        let ranks = self
            .class_ranks
            .as_ref()
            .map(|ranks| {
                ranks
                    .iter()
                    .map(|(key, value)| (key.clone(), *value))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        super::check_ranks_finite(&ranks, self.class_rank_unknown)
    }
}

/// Check structured ranks without losing non-finite numbers to JSON.
///
/// # Errors
/// Returns a diagnostic naming the keyword for invalid types, or the shared
/// rank diagnostic for a non-finite priority.
pub fn validate_rank_syntax<'de>(
    deserializer: impl serde::Deserializer<'de>,
) -> Result<(), ConfigError> {
    let syntax: ClassRankSyntax =
        serde_path_to_error::deserialize(deserializer).map_err(|e| error("rank syntax", e))?;
    syntax.validate()
}

fn error(option: &'static str, error: impl std::fmt::Display) -> ConfigError {
    ConfigError::InvalidSpec {
        option,
        message: format!("{option}: {error}"),
    }
}

fn take<T: DeserializeOwned + Default>(
    values: &mut Map<String, Value>,
    key: &'static str,
) -> Result<T, ConfigError> {
    values
        .remove(key)
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| error(key, e))
        .map(Option::unwrap_or_default)
}

/// Overlay keywords on a core default. A nested core group owns its leaves:
/// finding it uses its serde names, so a new ordinary field needs no wiring.
fn overlay(base: &mut Map<String, Value>, values: &mut Map<String, Value>) {
    for (name, value) in base.iter_mut() {
        if let Some(nested) = value.as_object_mut() {
            overlay(nested, values);
        } else if let Some(replacement) = values.remove(name) {
            *value = replacement;
        }
    }
}

pub(super) fn deserialize<T: Serialize + DeserializeOwned>(
    base: T,
    mut values: Map<String, Value>,
) -> Result<T, ConfigError> {
    let mut base = serde_json::to_value(base).map_err(|e| error("options", e))?;
    let original = values.clone();
    let defaults = base.clone();
    overlay(
        base.as_object_mut().expect("options serialize as objects"),
        &mut values,
    );
    // Let serde's deny_unknown_fields diagnose leftover keys.
    base.as_object_mut()
        .expect("options serialize as objects")
        .extend(values);
    serde_json::from_value(base).map_err(|error_value| {
        for (key, value) in original {
            let mut candidate = defaults.clone();
            let mut supplied = Map::from_iter([(key.clone(), value)]);
            overlay(
                candidate.as_object_mut().expect("options are objects"),
                &mut supplied,
            );
            candidate
                .as_object_mut()
                .expect("options are objects")
                .extend(supplied);
            if let Err(detail) = serde_json::from_value::<T>(candidate) {
                return error("options", format!("{key}: {detail}"));
            }
        }
        error("options", error_value)
    })
}

/// Parse convert syntax, then deserialize ordinary keywords into core options.
///
/// # Panics
/// Panics if the core override struct stops serializing as an object.
///
/// # Errors
/// Returns the core diagnostic for unknown keywords, invalid syntax or conflicting options.
pub fn convert(
    mut values: Map<String, Value>,
    mut base: ConvertOptions,
) -> Result<ConvertOptions, ConvertError> {
    let verbatim = take::<bool>(&mut values, "verbatim")?;
    if let Some(mode) = values.get("mode") {
        base.mode = serde_json::from_value(mode.clone()).map_err(|e| error("mode", e))?;
    }
    if let Some(cluster) = values.get("cluster") {
        base.cluster = serde_json::from_value(cluster.clone()).map_err(|e| error("cluster", e))?;
    }
    let mut overrides = Map::new();
    // GeneralizationOverrides already defines the optional override syntax.
    for key in serde_json::to_value(GeneralizationOverrides::default())
        .map_err(|e| error("generalization", e))?
        .as_object()
        .expect("overrides are an object")
        .keys()
    {
        if let Some(value) = values.remove(key).filter(|v| !v.is_null()) {
            overrides.insert(key.clone(), value);
        }
    }
    let overrides: GeneralizationOverrides =
        serde_path_to_error::deserialize(Value::Object(overrides))
            .map_err(|e| error("generalization", e))?;
    base = base.with_generalization(&overrides, verbatim)?;

    let collapse = take(&mut values, "collapse")?;
    let square = take(&mut values, "collapse_square")?;
    let no_collapse = take(&mut values, "no_collapse")?;
    if collapse || square || no_collapse {
        base.simplify.collapse = collapse_mode(collapse, square, no_collapse)?;
    }
    let ladder: Option<String> = take(&mut values, "magnitude_ladder")?;
    let step = values
        .remove("ladder_step")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| error("ladder_step", e))?
        .unwrap_or(super::DEFAULT_LADDER_STEP);
    let entry: Option<String> = take(&mut values, "entry_zoom")?;
    if ladder.is_some() || entry.is_some() {
        base.entry_zoom = entry_zoom(ladder.as_deref(), step, entry.as_deref())?;
    }
    let spec: Option<String> = take(&mut values, "class_rank")?;
    let column: Option<String> = take(&mut values, "class_rank_column")?;
    let ranks: Option<std::collections::BTreeMap<String, f64>> = take(&mut values, "class_ranks")?;
    let unknown: Option<f64> = take(&mut values, "class_rank_unknown")?;
    if spec.is_some() || column.is_some() || ranks.is_some() || unknown.is_some() {
        base.class_ranking = class_ranking(spec.as_deref(), column, ranks, unknown)?;
    }
    if values.contains_key("representation") {
        let spec: Option<String> = take(&mut values, "representation")?;
        base.representation = representation(spec.as_deref())?;
    }
    let gsds: Option<Vec<f64>> = take(&mut values, "gsds")?;
    let (min, max) = match base.levels {
        LevelPlan::ZoomRange { min_zoom, max_zoom } => (min_zoom, max_zoom),
        LevelPlan::Gsds(_) => (0, 6),
    };
    let min_zoom = values
        .remove("min_zoom")
        .map(serde_json::from_value)
        .transpose()
        .map_err(|e| error("min_zoom", e))?
        .unwrap_or(min);
    // `"auto"` builds the plan with the placeholder zoom and leaves the
    // estimate to the conversion, over its own source (#444, #788).
    let max_zoom = values
        .remove("max_zoom")
        .map(max_zoom_value)
        .transpose()?
        .unwrap_or(crate::overview::auto_zoom::MaxZoom::Fixed(max));
    let auto_max_zoom = max_zoom.is_auto();
    let max_zoom = max_zoom.plan_zoom();
    if let Some(gsds) = gsds {
        base.levels = LevelPlan::Gsds(gsds);
    } else if matches!(base.levels, LevelPlan::ZoomRange { .. }) {
        base.levels = LevelPlan::ZoomRange { min_zoom, max_zoom };
    }
    // The core-default value in a Python signature is informational; omitted
    // arguments stay absent, preserving cluster and verbatim precedence.
    let mut options: ConvertOptions = deserialize(base, values).map_err(ConvertError::from)?;
    // Set after the merge: the flag is not a keyword of its own, so the
    // serde round trip in `deserialize` drops it.
    options.auto_max_zoom = auto_max_zoom;
    Ok(options)
}

/// Resolve automatic size caps against this command's core defaults.
///
/// # Errors
/// Returns the core diagnostic for unknown keywords, invalid syntax or conflicting options.
pub fn export(
    mut values: Map<String, Value>,
    base: ExportOptions,
) -> Result<ExportOptions, ConfigError> {
    for (key, default) in [
        ("tile_size_limit", base.tile_size_limit),
        ("max_compressed_tile_size", base.compressed_tile_size_limit),
    ] {
        if let Some(value) = values.remove(key) {
            let cap: SizeCap = serde_json::from_value(value).map_err(|e| error(key, e))?;
            values.insert(
                key.into(),
                serde_json::to_value(cap.or(default)).map_err(|e| error(key, e))?,
            );
        }
    }
    deserialize(base, values)
}

/// Flatten serde groups for public keyword defaults and generated signatures.
///
/// # Panics
/// Panics if a core option struct fails serialization as an object.
pub fn defaults<T: Serialize>(options: T) -> Map<String, Value> {
    fn flatten(value: Value, out: &mut Map<String, Value>) {
        for (key, value) in value.as_object().expect("options are objects") {
            if value.is_object() && key != "accumulate_attributes" && key != "class_ranking" {
                flatten(value.clone(), out);
            } else {
                out.insert(key.clone(), value.clone());
            }
        }
    }
    let mut out = Map::new();
    flatten(
        serde_json::to_value(options).expect("options serialize"),
        &mut out,
    );
    out
}

fn group(values: &mut Map<String, Value>, names: Map<String, Value>) -> Map<String, Value> {
    names
        .keys()
        .filter_map(|key| values.remove(key).map(|value| (key.clone(), value)))
        .collect()
}

/// The legacy positional spellings accepted by Python's convert function.
pub const CONVERT_POSITIONAL: &[&str] = &[
    "min_zoom",
    "max_zoom",
    "layer_name",
    "tile_size_limit",
    "simple_clip_fastpath",
    "feature_order",
    "spill_dir",
];

fn facade_defaults<T: Serialize>(options: T) -> Map<String, Value> {
    let mut own = serde_json::to_value(options)
        .expect("core options serialize")
        .as_object()
        .expect("core options are objects")
        .clone();
    for stage in ["convert", "export", "metadata"] {
        own.remove(stage);
    }
    defaults(own)
}

/// Public defaults, generated from the option structs and syntax adapters.
///
/// # Panics
/// Panics if a core option struct fails serialization as an object.
pub fn command_defaults(command: &str) -> Map<String, Value> {
    use crate::overview::tiles::TilesOptions;
    use crate::pyramid::PyramidOptions;
    let mut out = match command {
        "overview" => {
            let mut out = defaults(ConvertOptions::default());
            out.extend(defaults(ConvertSyntax::default()));
            out.extend(defaults(GeneralizationOverrides::default()));
            out.remove("class_ranking");
            out.remove("class_rank");
            out.remove("explicit_coalesce_lines");
            out.remove("collapse_mode");
            out.remove("shard");
            out.remove("zoom_ceiling");
            out.insert("gsds".into(), Value::Null);
            out.insert("class_rank_column".into(), Value::Null);
            out.insert("class_ranks".into(), Value::Null);
            out.insert("class_rank_unknown".into(), Value::Null);
            out
        }
        "export_pmtiles" => {
            let mut out = defaults(ExportOptions::default());
            out.insert("tile_size_limit".into(), "auto".into());
            out.insert("max_compressed_tile_size".into(), "auto".into());
            out
        }
        "convert" => {
            let mut out = command_defaults("overview");
            out.remove("mode");
            out.remove("cogp_compat");
            let mut export = command_defaults("export_pmtiles");
            export.remove("min_zoom");
            export.remove("zoom_ceiling");
            // Property selection and spill are owned by the convert stage.
            for key in defaults(ConvertOptions::default()).keys() {
                export.remove(key);
            }
            out.extend(export);
            let tiles = TilesOptions::default();
            out.insert(
                "max_zoom".into(),
                match &tiles.convert.levels {
                    LevelPlan::ZoomRange { max_zoom, .. } => (*max_zoom).into(),
                    LevelPlan::Gsds(_) => unreachable!(),
                },
            );
            out.extend(facade_defaults(tiles));
            out
        }
        "pyramid" => {
            let mut out = command_defaults("export_pmtiles");
            for key in [
                "layer_name",
                "min_zoom",
                "tile_range",
                "zoom_ceiling",
                "spill_dir",
            ] {
                out.remove(key);
            }
            out.extend(facade_defaults(PyramidOptions::default()));
            out.insert("generalize".into(), false.into());
            out
        }
        "decode" => Map::from_iter([
            ("min_zoom".into(), Value::Null),
            ("max_zoom".into(), Value::Null),
            ("layer".into(), Value::Null),
        ]),
        _ => Map::new(),
    };
    // The tagged engine enum is internal; public level syntax is above.
    out.remove("ZoomRange");
    out
}

/// Parse a `max_zoom` keyword (#444): an int zoom, or `"auto"` in any case.
fn max_zoom_value(value: Value) -> Result<crate::overview::auto_zoom::MaxZoom, ConvertError> {
    use crate::overview::auto_zoom::MaxZoom;
    if let Some(s) = value.as_str() {
        if !s.eq_ignore_ascii_case("auto") {
            return Err(error("max_zoom", "must be an int or \"auto\"").into());
        }
        return Ok(MaxZoom::Auto);
    }
    value
        .as_u64()
        .and_then(|n| u8::try_from(n).ok())
        .map(MaxZoom::Fixed)
        .ok_or_else(|| error("max_zoom", "must be an int between 0 and 255 or \"auto\"").into())
}

/// Build the complete tiles configuration from a flat keyword call.
///
/// # Errors
/// Returns the core diagnostic for unknown keywords, invalid syntax or conflicting options.
pub fn tiles(
    mut values: Map<String, Value>,
) -> Result<crate::overview::tiles::TilesOptions, crate::overview::tiles::TilesError> {
    use crate::overview::tiles::TilesOptions;
    let base = TilesOptions::default();
    let verbatim = values
        .get("verbatim")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let convert = convert(
        group(&mut values, command_defaults("overview")),
        base.convert,
    )
    .map_err(crate::overview::tiles::TilesError::Convert)?;
    let export = export(
        group(
            &mut values,
            command_defaults("export_pmtiles")
                .into_iter()
                .filter(|(key, _)| {
                    ![
                        "layer_name",
                        "min_zoom",
                        "spill_dir",
                        "include_properties",
                        "exclude_properties",
                        "exclude_all_properties",
                    ]
                    .contains(&key.as_str())
                })
                .collect(),
        ),
        TilesOptions::export_defaults(verbatim),
    )?;
    // The auto flag is not a keyword, so the serde round trip below drops
    // it; carried across by hand.
    let auto_max_zoom = convert.auto_max_zoom;
    values.insert(
        "convert".into(),
        serde_json::to_value(convert).map_err(|e| error("convert", e))?,
    );
    values.insert(
        "export".into(),
        serde_json::to_value(export).map_err(|e| error("export", e))?,
    );
    let mut options: TilesOptions =
        serde_json::from_value(Value::Object(values)).map_err(|e| error("convert", e))?;
    options.convert.auto_max_zoom = auto_max_zoom;
    Ok(options)
}

/// Build the pyramid options, including verbatim band defaults and metadata.
///
/// # Errors
/// Returns the core diagnostic for unknown keywords, invalid syntax or conflicting options.
pub fn pyramid(
    mut values: Map<String, Value>,
) -> Result<crate::pyramid::PyramidOptions, ConfigError> {
    use crate::pyramid::PyramidOptions;
    let generalize: bool = take(&mut values, "generalize")?;
    let base = PyramidOptions::new(generalize);
    let metadata: crate::TilesetMetadata = deserialize(
        crate::TilesetMetadata::default(),
        group(&mut values, defaults(crate::TilesetMetadata::default())),
    )?;
    let export = export(
        group(&mut values, command_defaults("export_pmtiles")),
        base.export,
    )?;
    values.insert(
        "convert".into(),
        serde_json::to_value(base.convert).map_err(|e| error("convert", e))?,
    );
    values.insert(
        "export".into(),
        serde_json::to_value(export).map_err(|e| error("export", e))?,
    );
    values.insert(
        "metadata".into(),
        serde_json::to_value(metadata).map_err(|e| error("metadata", e))?,
    );
    serde_json::from_value(Value::Object(values)).map_err(|e| error("pyramid", e))
}

#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "cli", derive(clap::Args))]
pub struct ConvertSyntax {
    /// Disable thinning, simplification, and density reduction for
    /// pre-aggregated or pre-levelled input. Explicit tuning flags override
    /// these defaults.
    #[cfg_attr(feature = "cli", arg(long, help_heading = "Thinning & visibility"))]
    pub verbatim: bool,
    /// Rank `COL` values from highest to lowest, assigning entry zooms
    /// from `--min-zoom` in `--ladder-step` increments. Features appear at
    /// their entry zoom and finer zooms, exempt from thinning.
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "COL", help_heading = "Ranking")
    )]
    pub magnitude_ladder: Option<String>,
    /// Zooms between consecutive `--magnitude-ladder` rungs.
    #[cfg_attr(feature = "cli", arg(
        long,
        value_name = "N",
        default_value_t = super::DEFAULT_LADDER_STEP,
        help_heading = "Ranking",
        requires = "magnitude_ladder"
    ))]
    pub ladder_step: u8,
    /// Assign entry zooms: `COLUMN:VALUE=ZOOM,VALUE=ZOOM,...`, for example
    /// `density:5000=4,1000=6`. Unlisted values use the normal visibility gate.
    // Exclusive with --magnitude-ladder: core's rule
    // (`config::entry_zoom`), not clap's, so Python refuses it alike (#779).
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "SPEC", help_heading = "Ranking")
    )]
    pub entry_zoom: Option<String>,
    /// Rank categorical classes for cell winners: `COLUMN:VALUE=RANK,...`,
    /// where higher wins. Unlisted values rank below listed ones and above
    /// nulls.
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "SPEC", help_heading = "Ranking")
    )]
    pub class_rank: Option<String>,
    /// Replace polygons too small for a level with a point instead of the
    /// default placeholder square. Fill styles ignore points; add a circle
    /// layer.
    // The collapse trio's exclusivity is core's (`config::collapse_mode`).
    #[cfg_attr(feature = "cli", arg(long, help_heading = "Generalization"))]
    pub collapse: bool,
    /// Replace polygons too small for a level, and those thinned out at
    /// coarse levels, with small placeholder squares marking their area.
    /// This is the default in duplicating mode; output remains Polygon.
    #[cfg_attr(feature = "cli", arg(long, help_heading = "Generalization"))]
    pub collapse_square: bool,
    /// Drop polygons too small for a level instead of replacing them with
    /// placeholder squares: smaller files, but a polygon coverage shows holes
    /// at coarse zooms (the default before #745).
    #[cfg_attr(feature = "cli", arg(long, help_heading = "Generalization"))]
    pub no_collapse: bool,
    /// Polygon representation by zoom band: comma-separated
    /// `LO-HI:KIND`, where `KIND` is `geom`, `point`, or `square`, such as
    /// `0-7:point,8-14:geom`. Band rules:
    /// <https://geoparquet-io.github.io/tylertoo/OVERVIEW_TUNING/>.
    #[cfg_attr(
        feature = "cli",
        arg(long, value_name = "SPEC", help_heading = "Generalization")
    )]
    pub representation: Option<String>,
    /// Deprecated no-op: coalescing is now the default. Kept so existing
    /// invocations keep working; rejected with partitioning mode (where the
    /// default silently disables instead).
    #[cfg_attr(
        feature = "cli",
        arg(
            id = "explicit_coalesce_lines",
            long = "coalesce-lines",
            hide = true,
            conflicts_with = "coalesce_lines"
        )
    )]
    pub explicit_coalesce_lines: bool,
}

impl Default for ConvertSyntax {
    fn default() -> Self {
        Self {
            verbatim: false,
            magnitude_ladder: None,
            ladder_step: super::DEFAULT_LADDER_STEP,
            entry_zoom: None,
            class_rank: None,
            collapse: false,
            collapse_square: false,
            no_collapse: false,
            representation: None,
            explicit_coalesce_lines: false,
        }
    }
}

#[cfg(test)]
mod auto_max_zoom_tests {
    use super::*;

    fn keywords(v: Value) -> Map<String, Value> {
        v.as_object().expect("an object").clone()
    }

    /// #444/#788: `max_zoom="auto"` reaches the options as the auto flag
    /// with the placeholder zoom, through both serde round trips; a number
    /// clears it.
    #[test]
    fn max_zoom_auto_survives_the_keyword_round_trips() {
        use crate::overview::auto_zoom::MaxZoom;
        let auto = convert(
            keywords(serde_json::json!({"max_zoom": "AUTO"})),
            ConvertOptions::default(),
        )
        .unwrap();
        assert!(auto.auto_max_zoom);
        assert!(matches!(
            auto.levels,
            LevelPlan::ZoomRange { max_zoom, .. } if max_zoom == MaxZoom::Auto.plan_zoom()
        ));
        let fixed = convert(
            keywords(serde_json::json!({"max_zoom": 9})),
            ConvertOptions::default(),
        )
        .unwrap();
        assert!(!fixed.auto_max_zoom);
        assert!(
            tiles(keywords(serde_json::json!({"max_zoom": "auto"})))
                .unwrap()
                .convert
                .auto_max_zoom
        );
        assert!(convert(
            keywords(serde_json::json!({"max_zoom": "nope"})),
            ConvertOptions::default()
        )
        .is_err());
    }
}
