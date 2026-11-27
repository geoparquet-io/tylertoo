//! `--preset` / `preset=` (#822): another tool's defaults in place of
//! tylertoo's. A preset only replaces the core defaults that explicit flags
//! and keywords overlay, so anything the caller sets still wins. Its values
//! live here and nowhere else; `gen-reference-docs tippecanoe` renders them
//! into `crates/cli/docs/tippecanoe-tables.md`.

use crate::overview::convert::ConvertOptions;
use crate::overview::export::{ExportOptions, SizeValve, ValveScope};

/// A named set of option defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[non_exhaustive]
pub enum Preset {
    // tippecanoe's defaults; a doc comment here would become clap's
    // per-value help and print a "Possible values" list.
    Tippecanoe,
}

/// The tippecanoe release used for preset defaults and flag mappings.
pub const TIPPECANOE_VERSION: &str = "v2.82.0";

/// Sources for the preset's changed serde keys and their CLI flags.
///
/// References are to `felt/tippecanoe` at
/// `4f2621186acfec33b63ddf636f665623c0fef2dd`. The generator checks that
/// these keys equal the defaults changed by [`Preset`].
pub const TIPPECANOE_SOURCES: &[(&str, &str, &str)] = &[
    (
        "drop_gamma",
        "--drop-gamma",
        "main.cpp:3192 (gamma disabled; uniform thinning here)",
    ),
    (
        "simplify_factor",
        "--simplify-factor",
        "main.cpp:83,3707; geometry.cpp:220 (tile units converted to ground sample distance)",
    ),
    (
        "max_compressed_tile_size",
        "--max-compressed-tile-size",
        "main.cpp:85 (compressed bytes)",
    ),
    (
        "tile_size_limit",
        "--max-tile-size",
        "tile.cpp:2871-2891 (only compressed bytes are capped)",
    ),
    (
        "size_valve",
        "--size-valve",
        "tile.cpp:2867 (drop approximates detail reduction followed by failure)",
    ),
    (
        "valve_scope",
        "--valve-scope",
        "tile.cpp:3421-3422,3506-3509 (one gap threshold per zoom with density dropping)",
    ),
    (
        "max_zoom_simplify_factor",
        "--max-zoom-simplify-factor",
        "main.cpp:83,3707; geometry.cpp:220; tile.cpp:607 (also simplifies at maximum zoom)",
    ),
];

impl Preset {
    /// The convert defaults under `preset`, starting from `base`.
    #[must_use]
    pub fn convert(preset: Option<Self>, mut base: ConvertOptions) -> ConvertOptions {
        if preset == Some(Self::Tippecanoe) {
            // Unchanged by the preset; tippecanoe agrees (main.cpp:3191).
            base.density.drop_rate = 2.5;
            base.density.gamma = 1.0;
            base.simplify.factor = 0.25;
        }
        base
    }

    /// The export defaults under `preset`, starting from `base`.
    #[must_use]
    pub fn export(preset: Option<Self>, mut base: ExportOptions) -> ExportOptions {
        if preset == Some(Self::Tippecanoe) {
            // Unchanged by the preset; screen pixels (main.cpp:3193).
            base.tile_buffer = 5;
            base.compressed_tile_size_limit = Some(500_000);
            base.tile_size_limit = None;
            base.size_valve = SizeValve::Drop;
            base.valve_scope = ValveScope::Zoom;
            base.max_zoom_simplify_factor = Some(0.25);
        }
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overview::config::keywords;
    use crate::overview::tiles::TilesOptions;
    use serde_json::{json, Map, Value};

    fn map(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn tiles(value: Value) -> TilesOptions {
        keywords::tiles(map(value)).unwrap()
    }

    /// The nine values the tippecanoe preset sets, in one tuple.
    type PresetFields = (
        f64,
        f64,
        f64,
        u32,
        Option<usize>,
        Option<usize>,
        SizeValve,
        Option<f64>,
        ValveScope,
    );

    fn preset_fields(o: &TilesOptions) -> PresetFields {
        (
            o.convert.density.drop_rate,
            o.convert.density.gamma,
            o.convert.simplify.factor,
            o.export.tile_buffer,
            o.export.compressed_tile_size_limit,
            o.export.tile_size_limit,
            o.export.size_valve,
            o.export.max_zoom_simplify_factor,
            o.export.valve_scope,
        )
    }

    #[test]
    fn tippecanoe_sets_its_values_and_nothing_else() {
        let preset = tiles(json!({"preset": "tippecanoe"}));
        assert_eq!(
            preset_fields(&preset),
            (
                2.5,
                1.0,
                0.25,
                5,
                Some(500_000),
                None,
                SizeValve::Drop,
                Some(0.25),
                ValveScope::Zoom,
            )
        );
        let mut expected = tiles(json!({}));
        expected.convert.density.drop_rate = 2.5;
        expected.convert.density.gamma = 1.0;
        expected.convert.simplify.factor = 0.25;
        expected.export.tile_buffer = 5;
        expected.export.compressed_tile_size_limit = Some(500_000);
        expected.export.tile_size_limit = None;
        expected.export.size_valve = SizeValve::Drop;
        expected.export.max_zoom_simplify_factor = Some(0.25);
        expected.export.valve_scope = ValveScope::Zoom;
        assert_eq!(
            serde_json::to_value(&preset).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
    }

    #[test]
    fn no_preset_keeps_the_core_defaults() {
        let none = tiles(json!({"preset": null}));
        assert_eq!(
            serde_json::to_value(&none).unwrap(),
            serde_json::to_value(tiles(json!({}))).unwrap()
        );
    }

    /// Each explicit value wins over the preset's, and only that one moves.
    #[test]
    fn every_explicit_option_overrides_the_preset() {
        let base = preset_fields(&tiles(json!({"preset": "tippecanoe"})));
        for (key, value) in [
            ("drop_rate", json!(3.0)),
            ("drop_gamma", json!(2.0)),
            ("simplify_factor", json!(0.5)),
            ("tile_buffer", json!(12)),
            ("max_compressed_tile_size", json!(1_000_000)),
            ("tile_size_limit", json!(4_000_000)),
            ("size_valve", json!("merge")),
            ("max_zoom_simplify_factor", json!(0.5)),
            ("valve_scope", json!("smooth")),
        ] {
            let mut keywords = map(json!({"preset": "tippecanoe"}));
            keywords.insert(key.into(), value);
            let got = preset_fields(&tiles(Value::Object(keywords)));
            let mut expected = base;
            match key {
                "drop_rate" => expected.0 = 3.0,
                "drop_gamma" => expected.1 = 2.0,
                "simplify_factor" => expected.2 = 0.5,
                "tile_buffer" => expected.3 = 12,
                "max_compressed_tile_size" => expected.4 = Some(1_000_000),
                "tile_size_limit" => expected.5 = Some(4_000_000),
                "size_valve" => expected.6 = SizeValve::Merge,
                "max_zoom_simplify_factor" => expected.7 = Some(0.5),
                _ => expected.8 = ValveScope::Smooth,
            }
            assert_eq!(got, expected, "{key}");
        }
    }

    /// `auto` is the command's default cap, which the preset replaced.
    #[test]
    fn auto_caps_resolve_to_the_preset() {
        let got = tiles(json!({
            "preset": "tippecanoe",
            "max_compressed_tile_size": "auto",
            "tile_size_limit": "auto",
        }));
        assert_eq!(got.export.compressed_tile_size_limit, Some(500_000));
        assert_eq!(got.export.tile_size_limit, None);
    }

    /// `max_zoom_simplify_factor=None` is the command's default, as `auto`
    /// is for a cap: lines only, none under verbatim, both under the preset.
    #[test]
    fn a_null_max_zoom_simplify_factor_keeps_the_command_default() {
        let factor = |mut v: Value| {
            v["max_zoom_simplify_factor"] = Value::Null;
            tiles(v).export.max_zoom_simplify_factor
        };
        assert_eq!(factor(json!({})), None);
        assert_eq!(factor(json!({"verbatim": true})), Some(0.0));
        assert_eq!(factor(json!({"preset": "tippecanoe"})), Some(0.25));
    }

    /// The two halves resolve the same way when run as separate commands.
    #[test]
    fn overview_and_export_take_their_half() {
        let both = tiles(json!({"preset": "tippecanoe"}));
        let convert = keywords::convert(
            map(json!({"preset": "tippecanoe"})),
            ConvertOptions::default(),
        )
        .unwrap();
        assert_eq!(convert.density.drop_rate, both.convert.density.drop_rate);
        assert_eq!(convert.density.gamma, both.convert.density.gamma);
        assert_eq!(convert.simplify.factor, both.convert.simplify.factor);
        let export = keywords::export(
            map(json!({"preset": "tippecanoe"})),
            ExportOptions::default(),
        )
        .unwrap();
        assert_eq!(export.tile_buffer, both.export.tile_buffer);
        assert_eq!(
            (
                export.tile_size_limit,
                export.compressed_tile_size_limit,
                export.size_valve
            ),
            (
                both.export.tile_size_limit,
                both.export.compressed_tile_size_limit,
                both.export.size_valve
            )
        );
    }

    /// Verbatim turns off what the preset tunes; the pair is refused.
    #[test]
    fn verbatim_and_a_preset_are_refused() {
        let err =
            keywords::tiles(map(json!({"preset": "tippecanoe", "verbatim": true}))).unwrap_err();
        assert!(err.to_string().contains("--preset"), "{err}");
        assert_eq!(
            crate::error::Classify::kind(&err),
            crate::error::ErrorKind::Usage
        );
    }

    #[test]
    fn an_unknown_preset_names_the_known_ones() {
        let err = keywords::tiles(map(json!({"preset": "planetiler"}))).unwrap_err();
        assert!(err.to_string().contains("tippecanoe"), "{err}");
    }
}
