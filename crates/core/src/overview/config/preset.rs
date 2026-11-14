//! `--preset` / `preset=` (#822): another tool's defaults in place of
//! tylertoo's. A preset only replaces the core defaults that explicit flags
//! and keywords overlay, so anything the caller sets still wins. Its values
//! live here and nowhere else; `docs/guides/tippecanoe.md` tabulates them.

use crate::overview::convert::ConvertOptions;
use crate::overview::export::{ExportOptions, SizeValve};

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

impl Preset {
    /// The convert defaults under `preset`, starting from `base`.
    ///
    /// Line numbers cite tippecanoe v2.82.0 (`felt/tippecanoe` at
    /// `4f26211`); manby's `Options::default` (`src/options.rs`) agrees.
    #[must_use]
    pub fn convert(preset: Option<Self>, mut base: ConvertOptions) -> ConvertOptions {
        if preset == Some(Self::Tippecanoe) {
            // `-r` 2.5: `double droprate = 2.5;` (main.cpp:3191).
            base.density.drop_rate = 2.5;
            // `-g` 0, "for no effect" (main.cpp:3192, README): every
            // neighborhood loses the same share, which is gamma 1 here.
            base.density.gamma = 1.0;
            // `-S` 1 (main.cpp:83) is one tile unit at detail 12
            // (main.cpp:3707; geometry.cpp:220 `res = 1 << (32 - detail - z)`).
            // A tile unit is GSD x gsd-base / 4096, a quarter GSD at the
            // default `--gsd-base 1024`.
            base.simplify.factor = 0.25;
        }
        base
    }

    /// The export defaults under `preset`, starting from `base`.
    #[must_use]
    pub fn export(preset: Option<Self>, mut base: ExportOptions) -> ExportOptions {
        if preset == Some(Self::Tippecanoe) {
            // `-b` 5 screen pixels, 1/256 of a tile (main.cpp:3193, README).
            base.tile_buffer = 5;
            // `-M` 500,000 compressed bytes (main.cpp:85 `max_tile_size`).
            base.compressed_tile_size_limit = Some(500_000);
            // tippecanoe has no cap on the uncompressed tile.
            base.tile_size_limit = None;
            // tippecanoe's own default lowers the tile's detail and then
            // fails the tile, naming `--drop-densest-as-needed` (tile.cpp:2867);
            // tylertoo has no failing valve, so drop is the nearest.
            base.size_valve = SizeValve::Drop;
            // `-S` 1 also simplifies lines and polygons at the max zoom
            // (`-pS` / `--simplify-only-low-zooms` turns that off); 0.25 for
            // the reason `convert` gives above. Polygons part at shared
            // borders, as tippecanoe's do without `--detect-shared-borders`.
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

    /// The eight values the tippecanoe preset sets, in one tuple.
    type PresetFields = (
        f64,
        f64,
        f64,
        u32,
        Option<usize>,
        Option<usize>,
        SizeValve,
        Option<f64>,
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
                Some(0.25)
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
                _ => expected.7 = Some(0.5),
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
