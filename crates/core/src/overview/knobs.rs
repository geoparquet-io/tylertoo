//! The frontend-neutral input of the knobs every frontend shares (#779).
//!
//! The CLI and the Python bindings spell the convert and export knobs their
//! own way (`--no-cascade` against `cascade=False`, `--class-rank COL:V=R`
//! against a `class_ranks` dict). [`ConvertKnobs`] and [`ExportKnobs`] are
//! where those spellings meet core, and core alone turns them into options:
//! [`ConvertKnobs::convert_options`] and [`ExportKnobs::apply`].
//!
//! - The Python bindings deserialize a call's keyword arguments straight into
//!   these structs: the serde field names are the Python keyword names, and
//!   the defaults (`tylertoo._defaults()`) are their serialization.
//! - The CLI fills them from its flag groups, in one function per group.
//!
//! So a new knob is one field here, one CLI flag and one Python keyword;
//! neither frontend maps it onto an option by hand.
//!
//! Deserializing is strict where a Python caller's mistake is a type error:
//! a number is never a bool, a string is never a list of names.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::de::{self, Deserializer, Visitor};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};

use super::assign::{AssignConfig, BudgetKinds, DensityBudgetConfig, SortDirection};
use super::config::{self, ConfigError};
use super::convert::{
    parse_accumulate, ConvertError, ConvertOptions, GeneralizationOverrides, LevelPlan,
};
use super::export::{ExportOptions, FeatureOrder, SizeValve};
use super::level::{MemoryProfile, Mode};
use super::properties::PropertySelection;
use super::simplify::SimplifyOptions;
use super::writer::RowGroupSizePolicy;

/// A per-tile size cap as a caller states it: `auto` (the command's own core
/// default) or a byte count, `0` for no cap (#761). Python also takes `None`
/// for no cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SizeCap {
    /// The command's own default cap.
    #[default]
    Auto,
    /// This many bytes; `0` is no cap.
    Bytes(usize),
}

impl SizeCap {
    /// This cap over `default`, the command's own: `Auto` keeps it, `0`
    /// turns the cap off, any other size replaces it.
    pub fn or(self, default: Option<usize>) -> Option<usize> {
        match self {
            SizeCap::Auto => default,
            SizeCap::Bytes(n) => (n > 0).then_some(n),
        }
    }
}

impl Serialize for SizeCap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            SizeCap::Auto => s.serialize_str("auto"),
            SizeCap::Bytes(n) => s.serialize_u64(*n as u64),
        }
    }
}

impl<'de> Deserialize<'de> for SizeCap {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct CapVisitor;
        impl Visitor<'_> for CapVisitor {
            type Value = SizeCap;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("\"auto\", None or a byte count")
            }
            fn visit_str<E: de::Error>(self, s: &str) -> Result<SizeCap, E> {
                if s.trim().eq_ignore_ascii_case("auto") {
                    Ok(SizeCap::Auto)
                } else {
                    Err(E::invalid_value(de::Unexpected::Str(s), &self))
                }
            }
            fn visit_u64<E: de::Error>(self, n: u64) -> Result<SizeCap, E> {
                usize::try_from(n)
                    .map(SizeCap::Bytes)
                    .map_err(|_| E::invalid_value(de::Unexpected::Unsigned(n), &self))
            }
            fn visit_i64<E: de::Error>(self, n: i64) -> Result<SizeCap, E> {
                u64::try_from(n)
                    .map_err(|_| E::invalid_value(de::Unexpected::Signed(n), &self))
                    .and_then(|n| self.visit_u64(n))
            }
            fn visit_unit<E: de::Error>(self) -> Result<SizeCap, E> {
                Ok(SizeCap::Bytes(0))
            }
            fn visit_none<E: de::Error>(self) -> Result<SizeCap, E> {
                Ok(SizeCap::Bytes(0))
            }
        }
        d.deserialize_any(CapVisitor)
    }
}

/// The export knobs every command that writes tiles shares.
///
/// The CLI's `ExportArgs`, Python's export keywords. [`ExportKnobs::apply`]
/// lays them over a command's core defaults; validation is
/// [`ExportOptions::validate`]'s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExportKnobs {
    /// The raw MVT size cap ([`ExportOptions::tile_size_limit`]).
    pub tile_size_limit: SizeCap,
    /// The compressed size cap
    /// ([`ExportOptions::compressed_tile_size_limit`]).
    pub max_compressed_tile_size: SizeCap,
    /// [`ExportOptions::size_valve`].
    #[serde(with = "spelled")]
    pub size_valve: SizeValve,
    /// [`ExportOptions::coverage_fill`].
    #[serde(deserialize_with = "strict_bool")]
    pub coverage_fill: bool,
    /// [`ExportOptions::simple_clip_fastpath`].
    #[serde(deserialize_with = "strict_bool")]
    pub simple_clip_fastpath: bool,
    /// [`ExportOptions::tile_buffer`].
    pub tile_buffer: u32,
    /// [`ExportOptions::extent`].
    pub extent: u32,
    /// [`ExportOptions::partition_wave`].
    pub partition_wave: usize,
    /// [`ExportOptions::feature_order`].
    #[serde(with = "spelled")]
    pub feature_order: FeatureOrder,
    /// [`ExportOptions::feature_id`].
    pub feature_id: Option<String>,
}

impl Default for ExportKnobs {
    /// Core's export defaults; the caps are each command's own.
    fn default() -> Self {
        let e = ExportOptions::default();
        Self {
            tile_size_limit: SizeCap::Auto,
            max_compressed_tile_size: SizeCap::Auto,
            size_valve: e.size_valve,
            coverage_fill: e.coverage_fill,
            simple_clip_fastpath: e.simple_clip_fastpath,
            tile_buffer: e.tile_buffer,
            extent: e.extent,
            partition_wave: e.partition_wave,
            feature_order: e.feature_order,
            feature_id: e.feature_id,
        }
    }
}

impl ExportKnobs {
    /// These knobs over `export`, a command's core defaults.
    pub fn apply(self, export: ExportOptions) -> ExportOptions {
        ExportOptions {
            tile_size_limit: self.tile_size_limit.or(export.tile_size_limit),
            compressed_tile_size_limit: self
                .max_compressed_tile_size
                .or(export.compressed_tile_size_limit),
            size_valve: self.size_valve,
            coverage_fill: self.coverage_fill,
            simple_clip_fastpath: self.simple_clip_fastpath,
            tile_buffer: self.tile_buffer,
            extent: self.extent,
            partition_wave: self.partition_wave,
            feature_order: self.feature_order,
            feature_id: self.feature_id,
            ..export
        }
    }
}

/// The convert tuning knobs `overview` and `tiles` share.
///
/// Python's `overview()` and `convert()` take them too. Everything but the
/// mode, the level plan, the box and the `cogp` key, which each command owns.
///
/// Where the frontends spell one knob differently, this holds the meaning:
/// [`Self::density_drop`] is `None` for "not set", and a class ranking is
/// either the CLI's [`Self::class_rank`] spec or Python's structured parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConvertKnobs {
    /// The thinning winner's column ([`ConvertOptions::sort_key`]).
    pub sort_key: Option<String>,
    /// Which `sort_key` value wins. Python's spelling is case-insensitive.
    #[serde(
        serialize_with = "spelled::serialize",
        deserialize_with = "spelled::deserialize_lowercase"
    )]
    pub sort_direction: SortDirection,
    /// `--magnitude-ladder`'s column.
    pub magnitude_ladder: Option<String>,
    /// Zooms between `--magnitude-ladder` rungs.
    pub ladder_step: u8,
    /// `--entry-zoom`'s spec.
    pub entry_zoom: Option<String>,
    /// The CLI's `--class-rank COL:V=R,...` spec. Python spells the ranking
    /// as [`Self::class_rank_column`] and [`Self::class_ranks`] instead.
    #[serde(default, skip_serializing)]
    pub class_rank: Option<String>,
    /// The class ranking's column.
    pub class_rank_column: Option<String>,
    /// Each class's rank.
    pub class_ranks: Option<BTreeMap<String, f64>>,
    /// The rank of a class [`Self::class_ranks`] does not list.
    pub class_rank_unknown: Option<f64>,
    /// [`ConvertOptions::no_auto_rank`].
    #[serde(deserialize_with = "strict_bool")]
    pub no_auto_rank: bool,
    /// Turn every unset generalization knob off (#445).
    #[serde(deserialize_with = "strict_bool")]
    pub verbatim: bool,
    /// [`GeneralizationOverrides::simplify_factor`].
    pub simplify_factor: Option<f64>,
    /// `--collapse`.
    #[serde(deserialize_with = "strict_bool")]
    pub collapse: bool,
    /// `--collapse-square`.
    #[serde(deserialize_with = "strict_bool")]
    pub collapse_square: bool,
    /// `--no-collapse`.
    #[serde(deserialize_with = "strict_bool")]
    pub no_collapse: bool,
    /// `--representation`'s spec.
    pub representation: Option<String>,
    /// [`SimplifyOptions::cascade`].
    #[serde(deserialize_with = "strict_bool")]
    pub cascade: bool,
    /// [`GeneralizationOverrides::point_thinning`].
    pub point_thinning: Option<f64>,
    /// [`GeneralizationOverrides::line_thinning`].
    pub line_thinning: Option<f64>,
    /// [`GeneralizationOverrides::polygon_thinning`].
    pub polygon_thinning: Option<f64>,
    /// [`GeneralizationOverrides::line_visibility`].
    pub line_visibility: Option<f64>,
    /// [`GeneralizationOverrides::polygon_visibility`].
    pub polygon_visibility: Option<f64>,
    /// [`DensityBudgetConfig::drop_rate`].
    pub drop_rate: f64,
    /// [`DensityBudgetConfig::gamma`].
    pub drop_gamma: f64,
    /// [`GeneralizationOverrides::density_drop`]; `None` is not set.
    #[serde(deserialize_with = "strict_opt_bool")]
    pub density_drop: Option<bool>,
    /// Density-drop lines too.
    #[serde(deserialize_with = "strict_bool")]
    pub drop_lines: bool,
    /// Density-drop polygons too.
    #[serde(deserialize_with = "strict_bool")]
    pub drop_polygons: bool,
    /// [`ConvertOptions::cluster`].
    #[serde(deserialize_with = "strict_bool")]
    pub cluster: bool,
    /// `COL:OP` specs ([`parse_accumulate`]). Python passes `{COL: OP}`.
    #[serde(with = "accumulate")]
    pub accumulate_attributes: Vec<String>,
    /// [`GeneralizationOverrides::coalesce_lines`]; `None` is not set.
    #[serde(deserialize_with = "strict_opt_bool")]
    pub coalesce_lines: Option<bool>,
    /// [`ConvertOptions::coalesce_snap`].
    pub coalesce_snap: f64,
    /// [`ConvertOptions::coalesce_junction_angle`].
    pub coalesce_junction_angle: f64,
    /// [`ConvertOptions::coalesce_max_level_rows`].
    pub coalesce_max_level_rows: usize,
    /// [`PropertySelection::include`]; an empty list keeps none.
    #[serde(deserialize_with = "names")]
    pub include_properties: Option<Vec<String>>,
    /// [`PropertySelection::exclude`].
    #[serde(deserialize_with = "names")]
    pub exclude_properties: Option<Vec<String>>,
    /// [`PropertySelection::exclude_all`].
    #[serde(deserialize_with = "strict_bool")]
    pub exclude_all_properties: bool,
    /// [`ConvertOptions::gsd_base`].
    pub gsd_base: f64,
    /// [`ConvertOptions::max_row_group_size`].
    pub row_group_size: usize,
    /// [`ConvertOptions::row_group_size_policy`].
    #[serde(with = "spelled")]
    pub row_group_size_policy: RowGroupSizePolicy,
    /// [`ConvertOptions::full_column_stats`].
    #[serde(deserialize_with = "strict_bool")]
    pub full_column_stats: bool,
    /// [`ConvertOptions::streaming`].
    #[serde(deserialize_with = "strict_bool")]
    pub streaming: bool,
    /// [`ConvertOptions::read_batch_size`].
    pub read_batch_size: usize,
    /// [`ConvertOptions::filter`].
    pub filter: Option<String>,
    /// [`ConvertOptions::profile`].
    #[serde(with = "spelled")]
    pub profile: MemoryProfile,
    /// [`ConvertOptions::in_flight_batches`].
    pub in_flight_batches: usize,
    /// [`ConvertOptions::read_workers`].
    pub read_workers: usize,
    /// [`ConvertOptions::spill_dir`].
    pub spill_dir: Option<PathBuf>,
    /// [`ConvertOptions::save_plan`].
    pub save_plan: Option<PathBuf>,
    /// [`ConvertOptions::plan`].
    pub plan: Option<PathBuf>,
}

impl Default for ConvertKnobs {
    /// Core's convert defaults.
    fn default() -> Self {
        let c = ConvertOptions::default();
        Self {
            sort_key: c.sort_key,
            sort_direction: c.assign.sort_direction,
            magnitude_ladder: None,
            ladder_step: config::DEFAULT_LADDER_STEP,
            entry_zoom: None,
            class_rank: None,
            class_rank_column: None,
            class_ranks: None,
            class_rank_unknown: None,
            no_auto_rank: c.no_auto_rank,
            verbatim: false,
            simplify_factor: None,
            collapse: false,
            collapse_square: false,
            no_collapse: false,
            representation: None,
            cascade: c.simplify.cascade,
            point_thinning: None,
            line_thinning: None,
            polygon_thinning: None,
            line_visibility: None,
            polygon_visibility: None,
            drop_rate: c.density.drop_rate,
            drop_gamma: c.density.gamma,
            density_drop: None,
            drop_lines: c.density.kinds.lines,
            drop_polygons: c.density.kinds.polygons,
            cluster: c.cluster,
            accumulate_attributes: Vec::new(),
            coalesce_lines: None,
            coalesce_snap: c.coalesce_snap,
            coalesce_junction_angle: c.coalesce_junction_angle,
            coalesce_max_level_rows: c.coalesce_max_level_rows,
            include_properties: None,
            exclude_properties: None,
            exclude_all_properties: c.properties.exclude_all,
            gsd_base: c.gsd_base,
            row_group_size: c.max_row_group_size,
            row_group_size_policy: c.row_group_size_policy,
            full_column_stats: c.full_column_stats,
            streaming: c.streaming,
            read_batch_size: c.read_batch_size,
            filter: None,
            profile: c.profile,
            in_flight_batches: c.in_flight_batches,
            read_workers: c.read_workers,
            spill_dir: None,
            save_plan: None,
            plan: None,
        }
    }
}

impl ConvertKnobs {
    /// These knobs as [`ConvertOptions`], for a command that owns `mode`,
    /// `levels`, `bbox` and `cogp_compat`. Every rule is core's: the
    /// [`config`] helpers here, then [`ConvertOptions::with_generalization`]
    /// (the `verbatim` defaults); [`ConvertOptions::validate`] runs later.
    ///
    /// # Errors
    ///
    /// [`ConvertError::Config`] for a spec that does not parse or a
    /// conflicting combination, or what
    /// [`ConvertOptions::with_generalization`] refuses.
    pub fn convert_options(
        self,
        mode: Mode,
        levels: LevelPlan,
        bbox: Option<[f64; 4]>,
        cogp_compat: bool,
    ) -> Result<ConvertOptions, ConvertError> {
        let defaults = ConvertOptions::default();
        let overrides = GeneralizationOverrides {
            point_thinning: self.point_thinning,
            line_thinning: self.line_thinning,
            polygon_thinning: self.polygon_thinning,
            line_visibility: self.line_visibility,
            polygon_visibility: self.polygon_visibility,
            simplify_factor: self.simplify_factor,
            density_drop: self.density_drop,
            coalesce_lines: self.coalesce_lines,
        };
        let accumulate = self
            .accumulate_attributes
            .iter()
            .map(|spec| parse_accumulate(spec))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|message| ConfigError::InvalidSpec {
                option: "--accumulate-attribute",
                message,
            })?;
        let options = ConvertOptions {
            mode,
            levels,
            bbox,
            cogp_compat_key: cogp_compat,
            assign: AssignConfig {
                sort_direction: self.sort_direction,
                ..defaults.assign
            },
            sort_key: self.sort_key,
            entry_zoom: config::entry_zoom(
                self.magnitude_ladder.as_deref(),
                self.ladder_step,
                self.entry_zoom.as_deref(),
            )?,
            class_ranking: config::class_ranking(
                self.class_rank.as_deref(),
                self.class_rank_column,
                self.class_ranks,
                self.class_rank_unknown,
            )?,
            no_auto_rank: self.no_auto_rank,
            simplify: SimplifyOptions {
                collapse: config::collapse_mode(
                    self.collapse,
                    self.collapse_square,
                    self.no_collapse,
                )?,
                cascade: self.cascade,
                ..defaults.simplify
            },
            representation: config::representation(self.representation.as_deref())?,
            density: DensityBudgetConfig {
                drop_rate: self.drop_rate,
                gamma: self.drop_gamma,
                kinds: BudgetKinds {
                    points: true,
                    lines: self.drop_lines,
                    polygons: self.drop_polygons,
                },
                ..defaults.density
            },
            cluster: self.cluster,
            accumulate,
            coalesce_snap: self.coalesce_snap,
            coalesce_junction_angle: self.coalesce_junction_angle,
            coalesce_max_level_rows: self.coalesce_max_level_rows,
            properties: PropertySelection {
                include: self.include_properties,
                exclude: self.exclude_properties.unwrap_or_default(),
                exclude_all: self.exclude_all_properties,
            },
            gsd_base: self.gsd_base,
            max_row_group_size: self.row_group_size,
            row_group_size_policy: self.row_group_size_policy,
            full_column_stats: self.full_column_stats,
            streaming: self.streaming,
            read_batch_size: self.read_batch_size,
            filter: self.filter,
            profile: self.profile,
            in_flight_batches: self.in_flight_batches,
            read_workers: self.read_workers,
            spill_dir: self.spill_dir,
            save_plan: self.save_plan,
            plan: self.plan,
            // The shard (#498) and the convert-side ceiling (#541) are the
            // tiles facade's to set.
            ..defaults
        };
        // Last: the defaults read `cluster`, the logged summary `entry_zoom`.
        options.with_generalization(&overrides, self.verbatim)
    }
}

/// A value spelled as its `Display` / `FromStr` text.
mod spelled {
    use serde::{de, Deserialize, Deserializer, Serializer};
    use std::fmt::Display;
    use std::str::FromStr;

    pub fn serialize<S: Serializer, T: Display>(value: &T, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(value)
    }

    pub fn deserialize<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        parse(&String::deserialize(d)?)
    }

    /// [`deserialize`], ignoring case.
    pub fn deserialize_lowercase<'de, D, T>(d: D) -> Result<T, D::Error>
    where
        D: Deserializer<'de>,
        T: FromStr,
        T::Err: Display,
    {
        parse(&String::deserialize(d)?.to_lowercase())
    }

    fn parse<T: FromStr, E: de::Error>(s: &str) -> Result<T, E>
    where
        T::Err: Display,
    {
        s.parse().map_err(E::custom)
    }
}

/// A bool, and only a bool: never a number or a string.
struct StrictBool(bool);

impl<'de> Deserialize<'de> for StrictBool {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct BoolVisitor;
        impl Visitor<'_> for BoolVisitor {
            type Value = StrictBool;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a bool")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> Result<StrictBool, E> {
                Ok(StrictBool(v))
            }
        }
        d.deserialize_any(BoolVisitor)
    }
}

fn strict_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    StrictBool::deserialize(d).map(|b| b.0)
}

fn strict_opt_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
    Option::<StrictBool>::deserialize(d).map(|b| b.map(|b| b.0))
}

/// None, or a list of strings; with `pairs`, also a `{key: value}` mapping,
/// as `key:value` strings in key order. A lone string is never a list of
/// its characters.
struct Strings {
    expecting: &'static str,
    pairs: bool,
}

impl<'de> Visitor<'de> for Strings {
    type Value = Option<Vec<String>>;
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.expecting)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(None)
    }
    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut out = Vec::new();
        while let Some(item) = seq.next_element::<String>()? {
            out.push(item);
        }
        Ok(Some(out))
    }
    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        if !self.pairs {
            return Err(de::Error::invalid_type(de::Unexpected::Map, &self));
        }
        let mut pairs = BTreeMap::new();
        while let Some((key, value)) = map.next_entry::<String, String>()? {
            pairs.insert(key, value);
        }
        Ok(Some(
            pairs.into_iter().map(|(k, v)| format!("{k}:{v}")).collect(),
        ))
    }
}

/// A list of names, or none.
fn names<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vec<String>>, D::Error> {
    d.deserialize_any(Strings {
        expecting: "a list[str] or None",
        pairs: false,
    })
}

/// `COL:OP` specs, which Python passes as a `{COL: OP}` mapping (in column
/// order) and the CLI as a list.
mod accumulate {
    use serde::{Deserializer, Serializer};

    /// No specs serialize as none (Python's default), any as a list.
    pub fn serialize<S: Serializer>(specs: &[String], s: S) -> Result<S::Ok, S::Error> {
        if specs.is_empty() {
            s.serialize_none()
        } else {
            s.collect_seq(specs)
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
        let specs = d.deserialize_any(super::Strings {
            expecting: "a dict[str, str] of column to op, or None",
            pairs: true,
        })?;
        Ok(specs.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_knobs_build_the_default_options() {
        let built = ConvertKnobs::default()
            .convert_options(
                ConvertOptions::default().mode,
                ConvertOptions::default().levels,
                None,
                false,
            )
            .unwrap();
        let default = ConvertOptions::default()
            .with_generalization(&GeneralizationOverrides::default(), false)
            .unwrap();
        assert_eq!(format!("{built:?}"), format!("{default:?}"));
        assert_eq!(
            format!(
                "{:?}",
                ExportKnobs::default().apply(ExportOptions::default())
            ),
            format!("{:?}", ExportOptions::default())
        );
    }

    #[test]
    fn knobs_round_trip_through_their_serialized_defaults() {
        let json = serde_json::to_value(ConvertKnobs::default()).unwrap();
        assert!(json.get("class_rank").is_none(), "Python has no class_rank");
        assert_eq!(json["accumulate_attributes"], serde_json::Value::Null);
        let back: ConvertKnobs = serde_json::from_value(json).unwrap();
        assert_eq!(back, ConvertKnobs::default());
        let json = serde_json::to_value(ExportKnobs::default()).unwrap();
        assert_eq!(json["tile_size_limit"], "auto");
        let back: ExportKnobs = serde_json::from_value(json).unwrap();
        assert_eq!(back, ExportKnobs::default());
    }

    #[test]
    fn deserializing_is_strict_about_types() {
        let mut json = serde_json::to_value(ExportKnobs::default()).unwrap();
        json["coverage_fill"] = serde_json::json!(1);
        let err = serde_json::from_value::<ExportKnobs>(json.clone()).unwrap_err();
        assert!(err.to_string().starts_with("invalid type"), "{err}");
        json["coverage_fill"] = serde_json::json!(false);
        json["tile_size_limit"] = serde_json::json!(true);
        let err = serde_json::from_value::<ExportKnobs>(json.clone()).unwrap_err();
        assert!(err.to_string().starts_with("invalid type"), "{err}");
        json["tile_size_limit"] = serde_json::json!("lots");
        let err = serde_json::from_value::<ExportKnobs>(json.clone()).unwrap_err();
        assert!(err.to_string().starts_with("invalid value"), "{err}");
        json["tile_size_limit"] = serde_json::Value::Null;
        let knobs: ExportKnobs = serde_json::from_value(json).unwrap();
        assert_eq!(knobs.tile_size_limit, SizeCap::Bytes(0));

        let mut json = serde_json::to_value(ConvertKnobs::default()).unwrap();
        json["include_properties"] = serde_json::json!("name");
        let err = serde_json::from_value::<ConvertKnobs>(json).unwrap_err();
        assert!(err.to_string().contains("list[str]"), "{err}");
    }

    #[test]
    fn python_spellings_mean_what_the_cli_ones_do() {
        let mut json = serde_json::to_value(ConvertKnobs::default()).unwrap();
        json["sort_direction"] = serde_json::json!("ASC");
        json["accumulate_attributes"] = serde_json::json!({"pop": "sum", "area": "max"});
        let knobs: ConvertKnobs = serde_json::from_value(json).unwrap();
        assert_eq!(knobs.sort_direction, SortDirection::Asc);
        assert_eq!(knobs.accumulate_attributes, ["area:max", "pop:sum"]);
        let options = knobs
            .convert_options(Mode::Duplicating, LevelPlan::Gsds(vec![10.0]), None, false)
            .unwrap();
        assert_eq!(options.accumulate.len(), 2);

        let bad = ConvertKnobs {
            accumulate_attributes: vec!["pop".into()],
            ..ConvertKnobs::default()
        };
        let err = bad
            .convert_options(Mode::Duplicating, LevelPlan::Gsds(vec![10.0]), None, false)
            .unwrap_err();
        assert!(err.is_config(), "{err}");
    }
}
